# riverstone-voice-svc

A REST microservice backend for a voice assistant, handling appointment booking, call logging, and a basic knowledge base — built with Axum and SQLite.

> **Status: Work in progress.** Core routes are implemented; integration with a front-end voice interface is ongoing.

## Overview

`riverstone-voice-svc` is an Axum-based HTTP service that provides the backend plumbing for a voice assistant use case. It persists call records and appointments to a SQLite database, exposes a webhook endpoint for call-ended events, and serves a knowledge base route for assistant context.

All timestamps use the Melbourne timezone (AEST/AEDT) via `chrono-tz`.

## API Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/healthz` | Health check |
| `POST` | `/log_call` | Log an inbound or outbound call |
| `GET` | `/calls/recent` | Retrieve recent call records |
| `POST` | `/book_appointment` | Book an appointment |
| `POST` | `/webhooks/call/ended` | Webhook receiver for call-ended events |
| `GET` | `/kb` | Return knowledge base content for assistant context |

## Running

Copy `.env.example` to `.env` and set your database path, then:

```bash
cargo run
```

The server listens on `0.0.0.0:3000` by default (configurable via environment variables).

## Built with

- [`axum`](https://github.com/tokio-rs/axum) — async HTTP framework
- [`sqlx`](https://github.com/launchbadge/sqlx) — async SQLite persistence
- [`chrono-tz`](https://github.com/chronotope/chrono-tz) — timezone-aware timestamps (Melbourne/AEST)
- [`uuid`](https://github.com/uuid-rs/uuid) — v4 IDs for records
- [`tower-http`](https://github.com/tower-rs/tower-http) — tracing and request timeout middleware
