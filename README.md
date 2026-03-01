# HTTPTester

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Deploy Backend](https://github.com/SalindaGunarathna/httptester/actions/workflows/deploy-backend.yml/badge.svg)](https://github.com/SalindaGunarathna/httptester/actions/workflows/deploy-backend.yml)

A real-time webhook testing tool with browser push notifications. Generate a unique webhook URL, receive HTTP requests instantly in your browser, and inspect every detail — headers, body, and metadata. Zero server storage; your data stays local.

Security details are documented in `SECURITY_IMPLEMENTATION.md`.

## How It Works

1. **Subscribe** — Click Subscribe to grant notification permission and generate your unique webhook URL.
2. **Send requests** — Point any service (or use the built-in tester) to send HTTP requests to your webhook URL.
3. **Inspect** — Receive a push notification and inspect the full request right in your browser.

### Architecture

- Browser subscribes to Web Push and sends a `PushSubscription` to the server.
- Server stores only the subscription metadata and returns a short webhook URL.
- Any HTTP request sent to that URL is streamed, chunked, encrypted, and queued for push delivery.
- The browser decrypts and stores the webhook locally in IndexedDB.

### Streaming + Disk Queue

- The server **streams** request bodies and emits chunks as bytes arrive.
- Chunks are stored in a **bounded disk queue** (byte-capped).
- A fixed worker pool encrypts and delivers chunks via Web Push.
- Memory usage stays **predictable** under load and survives restarts.
- If the disk queue is full, the server returns **503**.
- If a sender disconnects mid-request, the UI may show a **partial delivery** after a short timeout.

## Tech Stack

- Rust 2024 + Axum
- `redb` embedded KV store (single file)
- `web-push` for RFC-compliant encryption and delivery
- `tokio` async runtime

## Setup

### Prerequisites

**Rust toolchain**

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```powershell
# Windows
winget install -e --id Rustlang.Rustup
```

**OpenSSL** (required by `web-push`)

- **macOS**: `brew install openssl`
- **Ubuntu/Debian**: `sudo apt-get install pkg-config libssl-dev`
- **Windows**: install via [vcpkg](https://github.com/microsoft/vcpkg):
  ```powershell
  cd C:\
  git clone https://github.com/microsoft/vcpkg
  .\vcpkg\bootstrap-vcpkg.bat
  .\vcpkg\vcpkg install openssl:x64-windows-static-md
  .\vcpkg\vcpkg integrate install
  setx VCPKG_ROOT "C:\vcpkg"
  ```

### Quick Start

1. Copy `.env.example` to `.env` and set your VAPID keys:
   ```bash
   cp .env.example .env
   npx web-push generate-vapid-keys
   ```
   Paste the generated keys into `VAPID_PUBLIC_KEY` and `VAPID_PRIVATE_KEY` in `.env`.

2. Start the server:
   ```bash
   cargo run
   ```

3. Open the UI at `http://localhost:3000`.

4. Subscribe from the browser, then send a test webhook:
   ```bash
   curl -X POST http://localhost:3000/hook/<uuid> \
     -H "Content-Type: application/json" \
     -d '{"hello":"world"}'
   ```

5. You can also test directly from the UI using the **Test Webhook** panel.

## Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/` | GET | Serves the frontend UI |
| `/health` | GET | Liveness check |
| `/api/config` | GET | Returns the VAPID public key |
| `/api/subscribe` | POST | Stores a `PushSubscription`, returns a webhook URL |
| `/api/subscribe/:uuid` | DELETE | Deletes a subscription (requires `X-Delete-Token` header) |
| `/hook/:uuid`, `/:uuid` | ANY | Accepts incoming webhooks |

### POST `/api/subscribe`

Request body:
```json
{
  "endpoint": "https://fcm.googleapis.com/fcm/send/...",
  "expirationTime": null,
  "keys": {
    "p256dh": "BNcRdreALRFXTkOOUHK1EtK2wtaz5Ry4YfYCA_0QTpQtUb...",
    "auth": "tBHItJI5svbpC7htP8Nw=="
  }
}
```

Response `200 OK`:
```json
{
  "uuid": "a1b2c3d4e5f6",
  "url": "http://localhost:3000/a1b2c3d4e5f6",
  "delete_token": "f1d2d2f924e986ac86fdf7b36c94bcdf"
}
```

### DELETE `/api/subscribe/:uuid`

- Requires header `X-Delete-Token`.
- `204` on success, `401` if token missing, `403` if invalid, `404` if UUID unknown.

### Webhook Ingestion (`/hook/:uuid`, `/:uuid`)

- Accepts any HTTP method.
- Streams, chunks, encrypts, and queues for Web Push delivery.
- `202 Accepted` — queued (delivery is async)
- `404 Not Found` — unknown UUID
- `413 Payload Too Large` — exceeds `MAX_PAYLOAD_BYTES`
- `429 Too Many Requests` — rate limit exceeded
- `503 Service Unavailable` — disk queue full
- `502 Bad Gateway` — push service rejected or subscription expired

## Environment Configuration

See `.env.example` for a full template.

**Required:**
- `VAPID_PUBLIC_KEY` — public VAPID key
- `VAPID_PRIVATE_KEY` — private VAPID key for signing

**Recommended:**
- `PUBLIC_BASE_URL` — public origin for webhook URLs (`http://localhost:3000` for dev, `https://...` for production)
- `CORS_ORIGINS` — comma-separated allowed frontend origins
- `ALLOWED_PUSH_HOSTS` — allowlist for push service endpoints

**Optional tuning:**

| Variable | Default |
|---|---|
| `DB_PATH` | `httptester.redb` |
| `QUEUE_DB_PATH` | `httptester.queue.redb` |
| `MAX_PAYLOAD_BYTES` | `102400` |
| `CHUNK_DATA_BYTES` | `2400` |
| `CHUNK_DELAY_MS` | `50` |
| `SUBSCRIPTION_TTL_DAYS` | `30` |
| `RATE_LIMIT_PER_MINUTE` | `60` |
| `WEBHOOK_READ_TIMEOUT_MS` | `3000` |
| `QUEUE_MAX_BYTES` | `1073741824` |
| `QUEUE_WORKERS` | `8` |
| `BIND_ADDR` | `0.0.0.0:3000` |
| `STATIC_DIR` | `frontend` |
| `SERVE_FRONTEND` | `true` |

## Cloudflare Worker (Static Assets + Router)

The Cloudflare Worker serves the frontend via static assets and proxies API/webhook paths to the backend, keeping a single origin (`https://httptester.com`).

1. Deploy using `cloudflare/worker.js` and `cloudflare/wrangler.toml`.
2. Set Worker variable: `BACKEND_ORIGIN=http://YOUR_SERVER_IP`
3. Route the Worker to `httptester.com/*`.
4. On the backend, set:
   - `PUBLIC_BASE_URL=https://httptester.com`
   - `CORS_ORIGINS=https://httptester.com`
   - `SERVE_FRONTEND=false`
5. Restart the backend service.

**Routing:** `/api/*`, `/hook/*`, `/health`, and `/:uuid` are proxied to the backend. Everything else is served by Worker assets.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Licensed under the [Apache License 2.0](LICENSE).
