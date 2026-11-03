use super::*;
use axum::{extract::Path, http::Uri, response::Response, routing::put};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    net::IpAddr,
    path::{Path as FilePath, PathBuf},
    time::Duration,
};
use tokio::sync::RwLock;
use tower::ServiceExt;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedProfile {
    id: String,
    label: String,
    environment: String,
    read_only: bool,
    host: String,
    port: u16,
    database: String,
    username: String,
    password: String,
    tls: String,
    ca_file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileForm {
    id: String,
    label: String,
    environment: String,
    #[serde(default = "default_read_only")]
    read_only: bool,
    host: String,
    port: u16,
    database: String,
    username: String,
    #[serde(default)]
    password: String,
    tls: String,
    ca_file: Option<String>,
}

#[derive(Serialize)]
struct ProfilePublic {
    id: String,
    label: String,
    environment: String,
    read_only: bool,
    host: String,
    port: u16,
    database: String,
    username: String,
    tls: String,
}

impl ManagedProfile {
    fn metadata(&self) -> ConnectionOption {
        ConnectionOption {
            id: self.id.clone(),
            label: self.label.clone(),
            engine: "postgresql".into(),
            environment: self.environment.clone(),
            read_only: self.read_only,
        }
    }
    fn public(&self) -> ProfilePublic {
        ProfilePublic {
            id: self.id.clone(),
            label: self.label.clone(),
            environment: self.environment.clone(),
            read_only: self.read_only,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            tls: self.tls.clone(),
        }
    }
    fn url(&self) -> String {
        let host = match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => format!("[{}]", self.host),
            _ => self.host.clone(),
        };
        format!(
            "postgres://{}:{}@{}:{}/{}",
            encode(&self.username),
            encode(&self.password),
            host,
            self.port,
            encode(&self.database)
        )
    }
}

fn encode(value: &str) -> String {
    let mut result = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            result.push(b as char)
        } else {
            result.push_str(&format!("%{b:02X}"))
        }
    }
    result
}

fn validate(p: &ManagedProfile) -> Result<(), &'static str> {
    if p.id.is_empty()
        || p.id.len() > 32
        || p.id == "default"
        || !p
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err("Invalid profile id");
    }
    if p.label.is_empty()
        || p.label.len() > 64
        || p.label.contains("://")
        || p.label.contains('@')
        || p.label.chars().any(char::is_control)
    {
        return Err("Invalid profile label");
    }
    if !matches!(p.environment.as_str(), "dev" | "test" | "prod") {
        return Err("Invalid environment");
    }
    if p.host.is_empty()
        || p.host.len() > 253
        || p.port == 0
        || !(p.host.parse::<IpAddr>().is_ok()
            || p.host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
    {
        return Err("Invalid PostgreSQL host or port");
    }
    if p.database.is_empty()
        || p.database.len() > 128
        || p.username.is_empty()
        || p.username.len() > 128
        || p.password.is_empty()
        || p.password.len() > 4096
        || p.database.chars().any(char::is_control)
        || p.username.chars().any(char::is_control)
    {
        return Err("Invalid database, username or password");
    }
    if !matches!(p.tls.as_str(), "disable" | "verify-full") {
        return Err("Invalid TLS mode");
    }
    if p.environment == "prod" && (!p.read_only || p.tls != "verify-full") {
        return Err("Production requires read-only and verified TLS");
    }
    if let Some(ca) = &p.ca_file {
        if ca.len() > 512 || !FilePath::new(ca).is_absolute() || p.tls != "verify-full" {
            return Err("Custom CA requires verified TLS and an absolute file path");
        }
    }
    if p.url().len() > 4096 {
        return Err("Profile connection string too long");
    }
    Ok(())
}

fn form_profile(
    form: ProfileForm,
    old: Option<&ManagedProfile>,
) -> Result<ManagedProfile, &'static str> {
    let p = ManagedProfile {
        id: form.id,
        label: form.label,
        environment: form.environment,
        read_only: form.read_only,
        host: form.host,
        port: form.port,
        database: form.database,
        username: form.username,
        password: if form.password.is_empty() {
            old.map(|p| p.password.clone()).unwrap_or_default()
        } else {
            form.password
        },
        tls: form.tls,
        ca_file: form.ca_file.or_else(|| old.and_then(|p| p.ca_file.clone())),
    };
    validate(&p)?;
    Ok(p)
}

pub(super) struct ManagedStore {
    path: PathBuf,
    static_ids: HashSet<String>,
    profiles: RwLock<Vec<ManagedProfile>>,
}

impl ManagedStore {
    pub(super) fn load(path: &str, existing: &[ConnectionOption]) -> Result<Self, String> {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("Profile store path must be absolute".into());
        }
        let parent = path.parent().ok_or("Missing profile store directory")?;
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|_| "Cannot create profile store directory")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .map_err(|_| "Cannot secure profile store directory")?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::symlink_metadata(parent)
                .map_err(|_| "Cannot inspect profile store directory")?;
            if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
                return Err("Profile store directory must not be group- or world-writable".into());
            }
        }
        let static_ids: HashSet<_> = existing.iter().map(|p| p.id.clone()).collect();
        let profiles = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err("Profile store must be a regular file".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err("Profile store requires 0600 permissions".into());
                    }
                }
                let bytes = fs::read(&path).map_err(|_| "Cannot read profile store")?;
                if bytes.len() > 64 * 1024 {
                    return Err("Profile store exceeds 64 KiB".into());
                }
                serde_json::from_slice::<Vec<ManagedProfile>>(&bytes)
                    .map_err(|_| "Invalid profile store JSON")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(_) => return Err("Cannot inspect profile store".into()),
        };
        Self::check(&profiles, &static_ids)?;
        Ok(Self {
            path,
            static_ids,
            profiles: RwLock::new(profiles),
        })
    }
    fn check(items: &[ManagedProfile], static_ids: &HashSet<String>) -> Result<(), String> {
        if items.len() + static_ids.len() > 8 {
            return Err("Maximum seven additional PostgreSQL profiles".into());
        }
        let mut seen = static_ids.clone();
        for item in items {
            validate(item).map_err(str::to_owned)?;
            if !seen.insert(item.id.clone()) {
                return Err("Duplicate profile id".into());
            }
        }
        Ok(())
    }
    fn persist(&self, items: &[ManagedProfile]) -> Result<(), String> {
        Self::check(items, &self.static_ids)?;
        let bytes = serde_json::to_vec(items).map_err(|_| "Cannot serialize profile store")?;
        if bytes.len() > 64 * 1024 {
            return Err("Profile store exceeds 64 KiB".into());
        }
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let operation = (|| -> std::io::Result<()> {
            let mut f = options.open(&temporary)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            if let Some(parent) = self.path.parent() {
                fs::File::open(parent)?.sync_all()?
            }
            Ok(())
        })();
        if operation.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        operation.map_err(|_| "Unable to securely persist connection profiles".into())
    }
    pub(super) async fn connection_options(&self) -> Vec<ConnectionOption> {
        self.profiles
            .read()
            .await
            .iter()
            .map(ManagedProfile::metadata)
            .collect()
    }
    async fn get(&self, id: &str) -> Option<ManagedProfile> {
        self.profiles
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }
}

#[derive(Serialize)]
struct ManagerListing {
    enabled: bool,
    profiles: Vec<ProfilePublic>,
}

async fn manager_list(State(state): State<AppState>) -> Json<ManagerListing> {
    let Some(store) = &state.managed else {
        return Json(ManagerListing {
            enabled: false,
            profiles: vec![],
        });
    };
    let profiles = store
        .profiles
        .read()
        .await
        .iter()
        .map(ManagedProfile::public)
        .collect();
    Json(ManagerListing {
        enabled: true,
        profiles,
    })
}

fn enabled(state: &AppState) -> Result<&Arc<ManagedStore>, (StatusCode, Json<ApiError>)> {
    state.managed.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connection Manager requires an access token and server-side store",
        )
    })
}

fn selected_state(
    base: &AppState,
    item: &ManagedProfile,
) -> Result<AppState, (StatusCode, Json<ApiError>)> {
    let tls = verified_pg_tls(&item.tls, item.ca_file.as_deref())
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid profile TLS configuration"))?;
    let mut selected = base.clone();
    selected.url = Arc::new(item.url());
    selected.tls = tls;
    selected.read_only = item.read_only;
    Ok(selected)
}

async fn verify(base: &AppState, item: &ManagedProfile) -> ApiResult<ConnectionStatus> {
    let selected = selected_state(base, item)?;
    let probe = async {
        let client = connect(&selected).await?;
        let row = client
            .query_one("SELECT current_database()::text, current_user::text", &[])
            .await
            .map_err(|_| connection_error())?;
        Ok(Json(ConnectionStatus {
            database: row.get(0),
            role: row.get(1),
            read_only: item.read_only,
        }))
    };
    tokio::time::timeout(Duration::from_secs(6), probe)
        .await
        .map_err(|_| api_error(StatusCode::GATEWAY_TIMEOUT, "Connection test timed out"))?
}

async fn manager_test(
    State(state): State<AppState>,
    Json(form): Json<ProfileForm>,
) -> ApiResult<ConnectionStatus> {
    let store = enabled(&state)?;
    let existing = store.get(&form.id).await;
    let candidate = form_profile(form, existing.as_ref())
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    verify(&state, &candidate).await
}

async fn manager_create(
    State(state): State<AppState>,
    Json(form): Json<ProfileForm>,
) -> ApiResult<ConnectionOption> {
    let store = enabled(&state)?;
    let mut profiles = store.profiles.write().await;
    if store.static_ids.contains(&form.id) || profiles.iter().any(|p| p.id == form.id) {
        return Err(api_error(StatusCode::CONFLICT, "Profile id already exists"));
    }
    let candidate =
        form_profile(form, None).map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    let _verified = verify(&state, &candidate).await?;
    let mut next = profiles.clone();
    next.push(candidate.clone());
    store
        .persist(&next)
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    *profiles = next;
    Ok(Json(candidate.metadata()))
}

async fn manager_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(form): Json<ProfileForm>,
) -> ApiResult<ConnectionOption> {
    let store = enabled(&state)?;
    if form.id != id {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Profile id cannot be changed",
        ));
    }
    let mut profiles = store.profiles.write().await;
    let index = profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Managed profile does not exist"))?;
    let candidate = form_profile(form, Some(&profiles[index]))
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    let _verified = verify(&state, &candidate).await?;
    let mut next = profiles.clone();
    next[index] = candidate.clone();
    store
        .persist(&next)
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    *profiles = next;
    Ok(Json(candidate.metadata()))
}

async fn manager_remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let store = enabled(&state)?;
    let mut profiles = store.profiles.write().await;
    if !profiles.iter().any(|p| p.id == id) {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Managed profile does not exist",
        ));
    }
    let next: Vec<_> = profiles.iter().filter(|p| p.id != id).cloned().collect();
    store
        .persist(&next)
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, message))?;
    *profiles = next;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn routes(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/managed-connections",
            get(manager_list).post(manager_create),
        )
        .route("/api/managed-connections/test", post(manager_test))
        .route(
            "/api/managed-connections/{id}",
            put(manager_update).delete(manager_remove),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_access_token,
        ))
        .with_state(state)
}

pub(super) async fn dispatch(
    State(base): State<AppState>,
    Path((id, tail)): Path<(String, String)>,
    mut request: Request,
) -> Response {
    if !tail.trim_start_matches('/').starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(store) = &base.managed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(profile) = store.get(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(state) = selected_state(&base, &profile) else {
        return connection_error().into_response();
    };
    let prefix = format!("/profiles/{id}");
    let original = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let Some(target) = original.strip_prefix(&prefix) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(uri) = target.parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;
    api_routes(state)
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> ManagedProfile {
        ManagedProfile {
            id: "reader".into(),
            label: "Restricted reader".into(),
            environment: "test".into(),
            read_only: true,
            host: "localhost".into(),
            port: 5432,
            database: "data".into(),
            username: "readonly".into(),
            password: "secret value@".into(),
            tls: "disable".into(),
            ca_file: None,
        }
    }
    #[test]
    fn rejects_invalid_profiles_and_unsafe_prod_modes() {
        let mut p = example();
        assert!(validate(&p).is_ok());
        p.id = "../bad".into();
        assert!(validate(&p).is_err());
        p.id = "prod".into();
        p.environment = "prod".into();
        assert!(validate(&p).is_err());
        p.tls = "verify-full".into();
        assert!(validate(&p).is_ok());
        p.read_only = false;
        assert!(validate(&p).is_err());
    }
    #[test]
    fn public_profile_never_contains_password_or_dsn() {
        let p = example();
        let json = serde_json::to_string(&p.public()).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("postgres://"));
        assert!(p.url().contains("secret%20value%40"));
    }
}
