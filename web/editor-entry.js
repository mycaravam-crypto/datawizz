// CodeMirror 6 is built into a self-contained browser bundle. No CDN/network at runtime.
import {EditorView, basicSetup} from "codemirror";
import {EditorState} from "@codemirror/state";
import {sql, PostgreSQL} from "@codemirror/lang-sql";
import {autocompletion, startCompletion} from "@codemirror/autocomplete";
import {keymap} from "@codemirror/view";
import {indentWithTab} from "@codemirror/commands";

const editorTheme = EditorView.theme({
  "&": {fontSize:"13px",height:"236px",backgroundColor:"var(--bg)",color:"var(--text)"},
  ".cm-scroller":{fontFamily:"ui-monospace,SFMono-Regular,Consolas,monospace",overflow:"auto"},
  ".cm-content":{padding:"11px 8px",minHeight:"210px",caretColor:"var(--text)"},
  ".cm-gutters":{backgroundColor:"var(--raised)",borderRight:"1px solid var(--border)",color:"var(--muted)"},
  ".cm-activeLine":{backgroundColor:"rgba(128,144,180,.11)"},
  ".cm-activeLineGutter":{backgroundColor:"rgba(128,144,180,.15)"},
  ".cm-selectionBackground":{backgroundColor:"rgba(100,140,230,.3)!important"},
  ".cm-tooltip":{backgroundColor:"var(--panel)",color:"var(--text)",border:"1px solid var(--border)"},
  ".cm-tooltip-autocomplete ul li[aria-selected]":{backgroundColor:"var(--accent-bg)",color:"var(--text)"}
});
function completionSource(suggest){
  return context=>{
    const before=context.matchBefore(/[A-Za-z_][A-Za-z_0-9.]*/);
    if(!context.explicit&&!before)return null;
    const prefix=before?.text??"";
    const proposed=suggest(prefix).slice(0,40).map(item=>({
      label:String(item.label),apply:String(item.insert??item.label),
      type:item.kind==="column"?"property":item.kind==="keyword"?"keyword":"variable",
      detail:item.kind
    }));
    return {from:before?.from??context.pos,options:proposed,validFor:/[A-Za-z_0-9.]*/};
  };
}
window.DataWizzEditor={
  create(parent,value,{onChange,onExecute,suggest}){
    const extensions=[
      basicSetup,
      sql({dialect:PostgreSQL}),
      editorTheme,
      autocompletion({override:[completionSource(suggest)],activateOnTyping:true}),
      keymap.of([
        {key:"Mod-Enter",run:()=>{onExecute();return true}},
        {key:"Mod-Space",run:startCompletion},
        indentWithTab
      ]),
      EditorView.updateListener.of(update=>{
        if(update.docChanged)onChange(update.state.doc.toString());
      })
    ];
    const view=new EditorView({state:EditorState.create({doc:value,extensions}),parent});
    view.contentDOM.setAttribute("aria-label","SQL statement editor");
    // Capture before CodeMirror's own keymaps: preserve DataWizz's established
    // Ctrl/Cmd+Enter execution shortcut regardless of internal command order.
    view.dom.addEventListener("keydown", event => {
      if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
        event.preventDefault();
        event.stopImmediatePropagation();
        onExecute();
      }
    }, true);
    return {
      getValue:()=>view.state.doc.toString(),
      setValue(text){
        const value=String(text);
        if(value!==view.state.doc.toString())view.dispatch({
          changes:{from:0,to:view.state.doc.length,insert:value}
        });
      },
      focus:()=>view.focus(),
      complete:()=>startCompletion(view),
      destroy:()=>view.destroy()
    };
  }
};
