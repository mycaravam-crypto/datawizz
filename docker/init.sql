-- Disposable sample data for local development only.
CREATE TABLE public.products (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL,
    price numeric(10,2) NOT NULL,
    available boolean NOT NULL DEFAULT true
);
INSERT INTO public.products (name, price, available) VALUES
    ('Mechanical keyboard', 89.00, true),
    ('USB-C hub', 29.90, true),
    ('Laptop stand', 44.50, false);

CREATE VIEW public.available_products AS
SELECT id, name, price FROM public.products WHERE available;
