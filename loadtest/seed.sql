-- 500k customers and 5M orders, indexed as a real schema would be.
CREATE TABLE customer (
  id BIGINT PRIMARY KEY,
  version BIGINT NOT NULL DEFAULT 1,
  name TEXT NOT NULL,
  country TEXT NOT NULL,
  city TEXT,
  created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE orders (
  id BIGINT PRIMARY KEY,
  customer_id BIGINT NOT NULL REFERENCES customer (id),
  status TEXT NOT NULL,
  total NUMERIC(12, 2) NOT NULL,
  placed_at TIMESTAMPTZ NOT NULL
);
SELECT setseed(0.42);
INSERT INTO customer (id, name, country, city, created_at)
SELECT i, 'Customer ' || i,
  (ARRAY['DE','FR','US','GB','JP','BR','IN','CN','IT','ES'])[1 + (i % 10)],
  CASE WHEN i % 7 = 0 THEN NULL ELSE 'City ' || (i % 500) END,
  '2020-01-01'::timestamptz + (i % 1500) * interval '1 day'
FROM generate_series(1, 500000) AS i;
INSERT INTO orders (id, customer_id, status, total, placed_at)
SELECT i, 1 + floor(random() * 500000)::bigint,
  (ARRAY['open','paid','shipped','returned'])[1 + floor(random() * 4)::int],
  round((random() * 1000)::numeric, 2),
  '2020-01-01'::timestamptz + random() * interval '1500 days'
FROM generate_series(1, 5000000) AS i;
CREATE INDEX orders_customer ON orders (customer_id);
CREATE INDEX orders_placed ON orders (placed_at);
CREATE INDEX customer_country ON customer (country);
VACUUM ANALYZE;
