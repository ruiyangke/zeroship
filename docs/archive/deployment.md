# Production Deployment Guide

> **Last updated:** 2026-04-01 | Covers single-node and multi-node deployments

---

## Prerequisites

| Dependency | Version | Purpose |
|---|---|---|
| Rust | 1.82+ (2024 edition) | Build from source |
| Node.js or Bun | Node 22+ / Bun 1.x | Dashboard frontend build |
| SQLite | 3.35+ (bundled via rusqlite) | Default app registry + metering store |
| PostgreSQL | 14+ | Production app registry (multi-node) |

The appbase binary is statically linked against V8 (via deno_core) and SQLite (via rusqlite/sqlx). The only runtime dependency on the host is glibc (Debian bookworm or newer).

---

## Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `APPBASE_MASTER_KEY` | **Yes (production)** | `dev-master-key` | Master API key for admin endpoints. If unset, a warning is printed and the dev default is used. **Never run production without setting this.** |
| `DATABASE_URL` | No | `sqlite://data/apps.db?mode=rwc` | SQLx connection URL for the control plane registry. Supports `sqlite://` and `postgres://`. |
| `APPBASE_APP_*` | No | — | Per-app environment variables exposed to JS via `env.get()`. Example: `APPBASE_APP_OWM_KEY=abc123` becomes `env.get("owm_key")` in JS. |

---

## Building from Source

```bash
# Clone
git clone https://github.com/example/appbase.git
cd appbase

# Release build (optimized, ~5 min on first build due to V8)
cargo build --release --bin appbase

# The binary is at target/release/appbase
ls -lh target/release/appbase
```

The workspace contains 11 crates. `cargo build --release --bin appbase` builds only the CLI and its transitive dependencies.

### Building the Dashboard

```bash
cd web/dashboard
npm ci
npx vite build
# Output: dist/
```

The dashboard is a Vite + React SPA. Serve it via the `--static` flag or a reverse proxy.

---

## Running with SQLite (Single-Node / Development)

SQLite is the default backend for both the app registry (`data/apps.db`) and metering store (`data/metering.db`). No external database required.

```bash
# Minimal production start
APPBASE_MASTER_KEY="$(openssl rand -hex 32)" \
  ./target/release/appbase serve server.js --port=3000

# With config file and custom paths
APPBASE_MASTER_KEY="your-secret-key" \
  ./target/release/appbase serve server.js \
    --port=3000 \
    --db=appbase.db \
    --config=appbase.toml
```

SQLite databases are created automatically in the `data/` directory:

| File | Purpose |
|---|---|
| `data/apps.db` | App registry (code, plans, API keys) |
| `data/metering.db` | Usage counters, billing snapshots |
| `appbase.db` | Per-app plugin SQLite database (DB plugin) |

All SQLite databases use WAL mode for concurrent reads during writes.

---

## Running with PostgreSQL (Multi-Node Production)

For multi-node deployments, use PostgreSQL for the control plane registry. This allows multiple appbase nodes to share the same app registry and support hot-reload across instances.

```bash
# Postgres connection string
export DATABASE_URL="postgres://appbase:password@db.example.com:5432/appbase"
export APPBASE_MASTER_KEY="your-production-secret"

./target/release/appbase serve server.js --port=3000
```

The `SqlxRegistry` auto-detects the database type from the URL scheme (`sqlite://` or `postgres://`). Schema migrations run automatically on startup.

### PostgreSQL Setup

```sql
CREATE DATABASE appbase;
CREATE USER appbase WITH PASSWORD 'your-password';
GRANT ALL PRIVILEGES ON DATABASE appbase TO appbase;
```

---

## Reverse Proxy Setup (nginx)

Appbase supports subdomain-based app routing: `my-app.platform.dev` routes to app `my-app`. This requires a wildcard DNS record and a reverse proxy.

### nginx Configuration

```nginx
# /etc/nginx/sites-available/appbase
server {
    listen 80;
    server_name platform.dev *.platform.dev;

    # Redirect HTTP to HTTPS
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    server_name platform.dev *.platform.dev;

    ssl_certificate     /etc/letsencrypt/live/platform.dev/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/platform.dev/privkey.pem;

    # Proxy to appbase
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # WebSocket support (for future use)
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        # Timeouts matching appbase wall-time limit
        proxy_read_timeout 35s;
        proxy_send_timeout 35s;
    }

    # Restrict admin endpoints to internal networks
    location /_stats {
        allow 10.0.0.0/8;
        allow 172.16.0.0/12;
        allow 192.168.0.0/16;
        allow 127.0.0.1;
        deny all;
        proxy_pass http://127.0.0.1:3000;
    }

    location /_health {
        proxy_pass http://127.0.0.1:3000;
    }

    location /_usage {
        allow 10.0.0.0/8;
        allow 172.16.0.0/12;
        allow 192.168.0.0/16;
        allow 127.0.0.1;
        deny all;
        proxy_pass http://127.0.0.1:3000;
    }

    location /_apps/ {
        allow 10.0.0.0/8;
        allow 172.16.0.0/12;
        allow 192.168.0.0/16;
        allow 127.0.0.1;
        deny all;
        proxy_pass http://127.0.0.1:3000;
    }
}
```

### DNS

Set up a wildcard A/AAAA record:

```
*.platform.dev.  300  IN  A  203.0.113.10
platform.dev.    300  IN  A  203.0.113.10
```

---

## TLS with Let's Encrypt

Use certbot with the DNS challenge for wildcard certificates:

```bash
# Install certbot
sudo apt install certbot python3-certbot-nginx

# Obtain wildcard certificate (requires DNS challenge)
sudo certbot certonly \
  --manual \
  --preferred-challenges dns \
  -d "platform.dev" \
  -d "*.platform.dev"

# Or with Cloudflare DNS plugin (automated)
sudo certbot certonly \
  --dns-cloudflare \
  --dns-cloudflare-credentials /etc/letsencrypt/cloudflare.ini \
  -d "platform.dev" \
  -d "*.platform.dev"

# Auto-renewal (already set up by certbot, verify with)
sudo certbot renew --dry-run
```

---

## Systemd Service

Create `/etc/systemd/system/appbase.service`:

```ini
[Unit]
Description=appbase platform server
After=network.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=appbase
Group=appbase
WorkingDirectory=/opt/appbase
ExecStart=/opt/appbase/appbase serve /opt/appbase/server.js \
    --port=3000 \
    --config=/opt/appbase/appbase.toml

# Environment
Environment=APPBASE_MASTER_KEY=your-production-secret-here
Environment=DATABASE_URL=sqlite://data/apps.db?mode=rwc
# Or for Postgres:
# Environment=DATABASE_URL=postgres://appbase:password@localhost:5432/appbase

# Per-app secrets
Environment=APPBASE_APP_OWM_KEY=your-api-key

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/appbase/data
PrivateTmp=true
ProtectKernelTunables=true
ProtectControlGroups=true

# Resource limits
LimitNOFILE=65536
MemoryMax=2G

# Restart policy
Restart=on-failure
RestartSec=5
StartLimitBurst=3
StartLimitIntervalSec=60

[Install]
WantedBy=multi-user.target
```

```bash
# Setup
sudo useradd --system --no-create-home --shell /usr/sbin/nologin appbase
sudo mkdir -p /opt/appbase/data
sudo cp target/release/appbase /opt/appbase/
sudo cp server.js appbase.toml /opt/appbase/
sudo chown -R appbase:appbase /opt/appbase

# Enable and start
sudo systemctl daemon-reload
sudo systemctl enable appbase
sudo systemctl start appbase
sudo journalctl -u appbase -f
```

---

## Docker

A multi-stage Dockerfile is provided at the repository root. See `Dockerfile`.

```bash
# Build
docker build -t appbase .

# Run
docker run -d \
  --name appbase \
  -p 3000:3000 \
  -e APPBASE_MASTER_KEY="your-secret" \
  -v ./data:/app/data \
  -v ./server.js:/app/server.js:ro \
  appbase

# With Postgres
docker run -d \
  --name appbase \
  -p 3000:3000 \
  -e APPBASE_MASTER_KEY="your-secret" \
  -e DATABASE_URL="postgres://appbase:password@db:5432/appbase" \
  -v ./server.js:/app/server.js:ro \
  appbase
```

### Docker Compose Example

```yaml
version: "3.8"
services:
  appbase:
    build: .
    ports:
      - "3000:3000"
    environment:
      APPBASE_MASTER_KEY: "${APPBASE_MASTER_KEY}"
      DATABASE_URL: "postgres://appbase:password@db:5432/appbase"
    volumes:
      - ./server.js:/app/server.js:ro
      - appbase-data:/app/data
    depends_on:
      db:
        condition: service_healthy

  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: appbase
      POSTGRES_PASSWORD: password
      POSTGRES_DB: appbase
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U appbase"]
      interval: 5s
      timeout: 5s
      retries: 5

volumes:
  appbase-data:
  pgdata:
```

---

## Health Checks

### Liveness

```bash
curl -f http://localhost:3000/_health
# {"status":"ok"}
```

Use this for load balancer health checks, Kubernetes liveness probes, and uptime monitoring.

### Readiness (Isolate Pool)

```bash
curl -s http://localhost:3000/_stats | jq '.active_isolates'
```

The `/_stats` endpoint returns isolate pool statistics including active count, max capacity, per-app CPU time, request counts, and idle duration.

### Monitoring Endpoints Summary

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /_health` | None | Liveness check (returns `{"status":"ok"}`) |
| `GET /_stats` | None* | Isolate pool stats (active count, per-app metrics) |
| `GET /_usage` | None* | Aggregated usage counters for all apps |
| `GET /_apps/:id/usage` | None* | Per-app usage counters |

*These endpoints should be network-restricted via reverse proxy in production (see nginx config above).

---

## Monitoring

### Key Metrics to Watch

| Metric | Source | Alert Threshold |
|---|---|---|
| Active isolates | `/_stats -> active_isolates` | > 80% of `max_isolates` (default 1000) |
| Per-app CPU time | `/_stats -> apps[].total_cpu_ms` | Sustained high CPU indicates runaway scripts |
| Per-app idle time | `/_stats -> apps[].idle_secs` | Low idle = high traffic; very high idle = candidate for eviction |
| Request count | `/_usage -> {app}.requests` | Monitor for traffic spikes |
| CPU usage (microseconds) | `/_usage -> {app}.cpu_us` | Track against plan quotas |
| Egress bytes | `/_usage -> {app}.egress_bytes` | Monitor bandwidth consumption |
| Concurrent requests | `/_usage -> {app}.concurrent_requests` | Near 100 = concurrency limit pressure |
| Health status | `/_health` | Any non-200 response |

### Prometheus Integration

Expose metrics by polling the admin endpoints with a scraper or sidecar:

```bash
# Example: simple metric export script
while true; do
  curl -s http://localhost:3000/_stats > /tmp/appbase_stats.json
  curl -s http://localhost:3000/_usage > /tmp/appbase_usage.json
  sleep 15
done
```

### Log Output

Appbase logs to stderr. Key log prefixes:

| Prefix | Meaning |
|---|---|
| `[appbase]` | Core startup, config loading, shutdown |
| `[hot-reload]` | App version change detected and isolate evicted |
| `[metering]` | Flush, rollover, recovery events |
| `[billing]` | Spending reconciliation events |

Capture logs with journald (systemd) or Docker log driver.

---

## Backup

### SQLite (Single-Node)

SQLite databases use WAL mode. Safe backup methods:

```bash
# Option 1: sqlite3 .backup command (online, consistent)
sqlite3 /opt/appbase/data/apps.db ".backup /backup/apps-$(date +%Y%m%d).db"
sqlite3 /opt/appbase/data/metering.db ".backup /backup/metering-$(date +%Y%m%d).db"

# Option 2: Copy WAL files together (must copy all three)
cp /opt/appbase/data/apps.db /backup/
cp /opt/appbase/data/apps.db-wal /backup/
cp /opt/appbase/data/apps.db-shm /backup/

# Cron job (daily at 2 AM)
0 2 * * * sqlite3 /opt/appbase/data/apps.db ".backup /backup/apps-$(date +\%Y\%m\%d).db"
```

**Do not** use plain `cp` on the main `.db` file without also copying the WAL and SHM files -- the backup will be corrupted if there are uncommitted WAL pages.

### PostgreSQL

```bash
# Full dump
pg_dump -U appbase -h localhost appbase > /backup/appbase-$(date +%Y%m%d).sql

# Compressed
pg_dump -U appbase -h localhost -Fc appbase > /backup/appbase-$(date +%Y%m%d).dump

# Restore
pg_restore -U appbase -h localhost -d appbase /backup/appbase-20260401.dump
```

### What to Back Up

| Data | Location | Frequency |
|---|---|---|
| App registry (code, keys, plans) | `data/apps.db` or Postgres | Daily + before upgrades |
| Metering store (usage counters) | `data/metering.db` | Daily (loss = max 5s of counters) |
| Plugin databases | `appbase.db` (or `--db` path) | Daily if apps use DB plugin |
| Configuration | `appbase.toml` | Version-controlled |

---

## Scaling Considerations

### Single Node

A single appbase instance handles significant load due to the V8 isolate pool and atomic counter architecture:

- **Isolate pool:** Up to 1000 concurrent isolates (configurable via `appbase.toml`)
- **Request overhead:** < 3 microseconds per request (enforcement + metering)
- **V8 dispatch:** 50-500 microseconds
- **Throughput:** Thousands of RPC calls/second on a single core (JS execution is the bottleneck)

### Horizontal Scaling

For multi-node deployments:

1. **Shared registry:** Use PostgreSQL as the `DATABASE_URL` so all nodes share app code and configuration.
2. **Load balancer:** Place nodes behind a load balancer. Any node can serve any app (isolates are created on demand).
3. **Hot reload:** All nodes poll the registry every 5 seconds. Deploy once, and all nodes pick up the new code within 5 seconds.
4. **Metering:** Each node maintains its own atomic counters and SQLite metering store. For accurate cross-node quotas, use a shared Redis or Postgres metering backend (MeterStore trait supports pluggable backends).
5. **Sticky sessions:** Not required. Isolates are stateless between requests (except KV, which is per-isolate and ephemeral).

### Resource Sizing

| Deployment | CPU | RAM | Storage |
|---|---|---|---|
| Development | 1 core | 512 MB | 100 MB |
| Small production (< 50 apps) | 2 cores | 2 GB | 1 GB |
| Medium production (50-500 apps) | 4 cores | 8 GB | 10 GB |
| Large production (500+ apps) | 8+ cores | 16+ GB | 50+ GB |

V8 isolates consume approximately 2-10 MB each depending on app complexity. The `max_isolates` config and idle eviction (30s default) control memory pressure.

---

## Security Checklist

- [ ] **Set `APPBASE_MASTER_KEY`** to a strong random value (at least 32 bytes). Never use the default `dev-master-key` in production.
- [ ] **Restrict admin endpoints** (`/_stats`, `/_usage`, `/_apps/`) to internal networks via reverse proxy or firewall.
- [ ] **Use TLS** for all external traffic. Terminate TLS at the reverse proxy.
- [ ] **Run as non-root** user with minimal permissions (see systemd `User=appbase`).
- [ ] **Enable systemd hardening** (`NoNewPrivileges`, `ProtectSystem`, `ReadWritePaths`).
- [ ] **Set memory limits** (`MemoryMax` in systemd or Docker `--memory`) to prevent OOM from runaway isolates.
- [ ] **Set file descriptor limits** (`LimitNOFILE=65536`) to support high concurrency.
- [ ] **Back up databases** before upgrades and on a regular schedule.
- [ ] **Rotate API keys** periodically. Re-create apps or implement key rotation via the control plane API.
- [ ] **Review per-app environment variables** (`APPBASE_APP_*`). These are accessible to all JS code running under that prefix.
- [ ] **Network-restrict PostgreSQL** if used. Do not expose Postgres to the public internet.
- [ ] **Monitor spending limits**. The billing reconciler checks every 10 seconds. Set per-app spending limits in config to prevent surprise bills.
- [ ] **SSRF protection** is built-in: `fetch()` blocks private IP ranges (10.x, 192.168.x, 127.x, link-local). Verify your network topology does not expose internal services via public IPs.
- [ ] **Audit deployed code** via `GET /api/apps/:id` to inspect what JS is running on each app.
