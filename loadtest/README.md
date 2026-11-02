# Load test

Measures the service over HTTP against PostgreSQL with 500k customers and 5M orders. It isn't part
of `cargo test`.

```bash
docker run -d --name odatabase-load -e POSTGRES_PASSWORD=postgres -p 127.0.0.1:55432:5432 --shm-size=1g postgres:17-alpine -c shared_buffers=1GB -c work_mem=64MB -c max_wal_size=4GB
```

```bash
docker exec -i odatabase-load psql -q -U postgres < loadtest/seed.sql
```

```bash
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:55432/postgres cargo run --release -p odatabase --example load_server
```

```bash
python3 -I loadtest/run.py
```

```bash
docker rm -fv odatabase-load
```

`PAGE_SIZE` sets the server page size (default 1000, `none` for unlimited), `POOL_SIZE` the number
of connections (default 16) and `CAP_SECONDS` the client timeout (default 60).
