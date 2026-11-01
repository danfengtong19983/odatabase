CREATE TABLE business_partner (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  version BIGINT NOT NULL DEFAULT 1,
  name TEXT,
  rating INTEGER,
  revenue NUMERIC(12, 2),
  score DOUBLE PRECISION,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  abc TEXT,
  flags BIGINT,
  created_at TIMESTAMPTZ,
  founded DATE,
  external_id UUID,
  addr_street TEXT,
  addr_city TEXT,
  addr_country TEXT,
  owner_id TEXT REFERENCES business_partner (id),
  first_name TEXT,
  size INTEGER
);
CREATE TABLE partner_role (
  partner_id TEXT NOT NULL REFERENCES business_partner (id),
  code INTEGER NOT NULL,
  label TEXT,
  PRIMARY KEY (partner_id, code)
);
CREATE TABLE partner_comment (
  partner_id TEXT NOT NULL REFERENCES business_partner (id),
  position INTEGER NOT NULL,
  body TEXT NOT NULL,
  PRIMARY KEY (partner_id, position)
);
CREATE TABLE partner_phone (
  partner_id TEXT NOT NULL REFERENCES business_partner (id),
  position INTEGER NOT NULL,
  kind TEXT,
  number TEXT,
  PRIMARY KEY (partner_id, position)
);
CREATE TABLE project (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL
);
CREATE TABLE partner_project (
  partner_id TEXT NOT NULL REFERENCES business_partner (id),
  project_id INTEGER NOT NULL REFERENCES project (id),
  PRIMARY KEY (partner_id, project_id)
);
CREATE TABLE division (
  code TEXT PRIMARY KEY,
  parent_code TEXT REFERENCES division (code),
  population BIGINT
);
CREATE TABLE settings (
  id INTEGER PRIMARY KEY,
  theme TEXT
);
CREATE TABLE ticket (
  id SERIAL PRIMARY KEY,
  title TEXT NOT NULL,
  note TEXT,
  priority INTEGER,
  version INTEGER NOT NULL,
  project_id INTEGER REFERENCES project (id)
);
CREATE TABLE ticket_tag (
  ticket_id INTEGER NOT NULL REFERENCES ticket (id),
  position INTEGER NOT NULL,
  tag TEXT NOT NULL
);
INSERT INTO business_partner
  (id, kind, version, name, rating, revenue, score, active, abc, flags, created_at, founded, external_id,
   addr_street, addr_city, addr_country, owner_id, first_name, size)
VALUES
  ('1', 'O', 3, 'First Org', 5, 1200.50, 1.5, TRUE, 'A', 3, '2024-01-15T08:30:00Z', '1999-03-01',
   '01234567-89ab-cdef-0123-456789abcdef', 'Main 1', 'Berlin', 'DEU', NULL, NULL, 100),
  ('2', 'O', 1, 'Second Org', 3, 80.25, 2.5, FALSE, 'B', 1, '2024-06-01T12:00:00Z', '2010-07-20',
   NULL, 'Side 2', 'Paris', 'FRA', '1', NULL, 20),
  ('3', 'P', 1, 'Third O''Neil', NULL, NULL, NULL, TRUE, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, '1', 'Ann', NULL),
  ('4', 'P', 1, 'Fourth', 1, 10.00, 0.5, TRUE, 'C', 0, '2023-12-31T23:59:59Z', '2000-02-29',
   NULL, 'Lane 4', 'Berlin', 'DEU', NULL, 'Bob', NULL);
INSERT INTO partner_role (partner_id, code, label) VALUES
  ('1', 1, 'Buyer'), ('1', 2, 'Seller'), ('1', 3, 'Agent'), ('2', 1, 'Buyer'), ('3', 2, NULL);
INSERT INTO partner_comment (partner_id, position, body) VALUES
  ('1', 2, 'second'), ('1', 1, 'first'), ('2', 1, 'only');
INSERT INTO partner_phone (partner_id, position, kind, number) VALUES
  ('1', 1, 'work', '111'), ('1', 2, 'home', '222');
INSERT INTO project (id, title) VALUES (10, 'Alpha'), (20, 'Beta'), (30, 'Gamma');
INSERT INTO partner_project (partner_id, project_id) VALUES ('1', 10), ('1', 20), ('2', 20);
INSERT INTO division (code, parent_code, population) VALUES
  ('EU', NULL, 450), ('DE', 'EU', 83), ('BE', 'DE', 3), ('BY', 'DE', 13), ('FR', 'EU', 68);
INSERT INTO settings (id, theme) VALUES (1, 'dark');
CREATE TABLE secret (
  id INTEGER PRIMARY KEY,
  partner_id TEXT REFERENCES business_partner (id),
  tenant TEXT NOT NULL,
  level INTEGER NOT NULL,
  region TEXT,
  body TEXT,
  salary INTEGER
);
INSERT INTO secret (id, partner_id, tenant, level, region, body, salary) VALUES
  (1, '1', 'acme', 1, 'eu-west', 'a', 100),
  (2, '1', 'acme', 5, 'us-east', 'b', 200),
  (3, '2', 'globex', 2, 'eu-north', 'c', 300);
CREATE TABLE vehicle (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL
);
CREATE TABLE car (
  vehicle_id INTEGER PRIMARY KEY REFERENCES vehicle (id),
  doors INTEGER
);
CREATE TABLE truck (
  vehicle_id INTEGER PRIMARY KEY REFERENCES vehicle (id),
  payload DOUBLE PRECISION
);
INSERT INTO vehicle (id, name) VALUES (1, 'Plain'), (2, 'Coupe'), (3, 'Hauler');
INSERT INTO car (vehicle_id, doors) VALUES (2, 2);
INSERT INTO truck (vehicle_id, payload) VALUES (3, 7.5);
CREATE TABLE place (
  id INTEGER PRIMARY KEY,
  country TEXT,
  first_name TEXT,
  last_name TEXT,
  location TEXT,
  blurb TEXT
);
CREATE TABLE country_text (
  code TEXT NOT NULL,
  language TEXT NOT NULL,
  text TEXT NOT NULL,
  PRIMARY KEY (code, language)
);
INSERT INTO place (id, country, first_name, last_name, location, blurb) VALUES
  (1, 'DE', 'Ada', 'Byron', '{"type":"Point","coordinates":[13.4,52.5]}', 'red mountain bike'),
  (2, 'FR', 'Alan', 'Turing', NULL, 'blue road bike'),
  (3, 'XX', NULL, NULL, NULL, 'green Mountain tent');
INSERT INTO country_text (code, language, text) VALUES
  ('DE', 'en', 'Germany'), ('DE', 'de', 'Deutschland'), ('FR', 'en', 'France');
CREATE TABLE document (
  id INTEGER PRIMARY KEY,
  title TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  content BYTEA,
  mime TEXT
);
INSERT INTO document (id, title, content, mime) VALUES (1, 'empty', NULL, NULL);
CREATE TABLE typed (
  id INTEGER PRIMARY KEY,
  byte_v SMALLINT,
  sbyte_v SMALLINT,
  short_v SMALLINT,
  long_v BIGINT,
  single_v REAL,
  time_v TIME,
  duration_v BIGINT,
  binary_v BYTEA,
  guid_v UUID,
  date_v DATE,
  stamp_v TIMESTAMPTZ,
  amount_v NUMERIC(20, 6),
  flag_v BOOLEAN
);
CREATE TABLE locked (
  id INTEGER PRIMARY KEY,
  name TEXT,
  code TEXT
);
INSERT INTO locked (id, name, code) VALUES (1, 'a', 'x'), (2, 'b', 'y');
