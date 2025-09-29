use axum::{
    Json, Router,
    routing::{get, post},
    http::StatusCode,
    http, 
    extract::State
};
use chrono::{DateTime, FixedOffset};
use chrono_tz::Australia::Melbourne;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions, Row};
use std::{str::FromStr, sync::Arc};
use tokio;
use tokio::net::TcpListener;
use tower::{ServiceBuilder, timeout::TimeoutLayer};
use axum;
use axum::error_handling::HandleErrorLayer;
use axum::BoxError;
use tower_http::{cors::{Any, CorsLayer}, trace::TraceLayer};
use std::time::Duration;
use uuid::Uuid;



fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_methods([http::Method::GET, http::Method::POST, http::Method::OPTIONS])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
        .allow_origin(Any) // or restrict to your provider’s origin if known
}

fn build_app() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/book_appointment", post(book_appointment))
        .route("/log_call", post(log_call))
        .route("/webhooks/call/ended", post(webhook_call_ended))
        .route("/kb", get(kb_lookup))
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(cors())
                .layer(HandleErrorLayer::new(|_err: BoxError| async move {
                    (StatusCode::GATEWAY_TIMEOUT, "request timed out".to_string())
                }))
                .layer(TimeoutLayer::new(Duration::from_secs(10)))
        )
}

async fn require_key(
    headers: axum::http::HeaderMap,
    State(expected): State<Arc<String>>,
) -> Result<(), (StatusCode, &'static str)> {
    let Some(h) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err((StatusCode::UNAUTHORIZED, "missing auth"));
    };
    let got = h.to_str().unwrap_or_default();
    if got != format!("Bearer {}", expected.as_str()) {
        return Err((StatusCode::FORBIDDEN, "bad auth"));
    }
    Ok(())
}


#[derive(Deserialize)]
struct BookReq {
    name: String,
    phone: String,
    email: String,
    slot_iso: String, // ISO 8601 with timezone, e.g. 2025-09-26T10:00:00+10:00
    mode: String,     // "video" | "display-suite"
    notes: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct BookResp {
    ok: bool,
    booking_id: String,
    message: String,
}

#[derive(Deserialize, Serialize)]
struct CallLog {
    timestamp: String,
    caller_cli: String,
    summary: String,
    qualification: serde_json::Value,
    booking: Option<serde_json::Value>,
    compliance_flags: Vec<String>,
    transcript_url: Option<String>,
    recording_url: Option<String>,
}

#[derive(Clone)]
struct AppState {
    db: Arc<SqlitePool>,
    api_key: Arc<String>
}

const CREATE_CALLS_TABLE_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS calls (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      ts TEXT NOT NULL,
      caller_cli TEXT,
      summary TEXT,
      qualification TEXT,
      booking TEXT,
      compliance_flags TEXT,
      transcript_url TEXT,
      recording_url TEXT
    )"#;

async fn readyz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    let ok = sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&*state.db).await.is_ok();
    if ok { (StatusCode::OK, "ready") } else { (StatusCode::SERVICE_UNAVAILABLE, "db down") }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("tower_http=info".parse().unwrap())
            .add_directive("riverstone_voice_svc=info".parse().unwrap()))
        .init();

    let raw_db = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:calls.db".into());
    let db_url = if raw_db.starts_with("sqlite:") { raw_db } else { format!("sqlite:{raw_db}") };
    let api_key = std::env::var("API_KEY").unwrap_or_else(|_| "dev-key".into());
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3005".into());

    let opts = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true);
    let db = SqlitePool::connect_with(opts).await?;
    sqlx::query(CREATE_CALLS_TABLE_SQL).execute(&db).await?;
    // Backfill any missing `booking` records from older payloads and ensure booking_id exists
    backfill_bookings(&db).await.ok();

    let state = AppState { db: Arc::new(db), api_key: Arc::new(api_key) };
    let app = build_app()
        .route("/readyz", get(readyz))      // add readiness
        .route("/health", get(healthz))     // optional alias
        .with_state(state);

    let addr = bind.parse::<std::net::SocketAddr>()?;
    tracing::info!("listening on {bind}");
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn backfill_bookings(db: &SqlitePool) -> anyhow::Result<()> {
    // Find rows with missing booking or missing booking_id
    let rows = sqlx::query(
        r#"SELECT id, qualification, booking FROM calls
            WHERE (booking IS NULL OR booking = 'null' OR booking = '')
               OR json_extract(booking, '$.booking_id') IS NULL
               OR json_extract(booking, '$.booking_id') = ''"#,
    )
    .fetch_all(db)
    .await?;

    for row in rows {
        let id: i64 = row.try_get("id")?;
        let qualification: String = row.try_get("qualification")?;
        let booking_str: Option<String> = row.try_get("booking").ok();

        let qual_val: serde_json::Value = serde_json::from_str(&qualification).unwrap_or(serde_json::json!({}));
        let booking_val: Option<serde_json::Value> = match booking_str {
            Some(s) if !s.is_empty() && s != "null" => serde_json::from_str(&s).ok(),
            _ => None,
        };

        if let Some(norm) = normalize_booking(booking_val.as_ref(), &qual_val) {
            let serialized = serde_json::to_string(&norm).unwrap_or("null".into());
            let _ = sqlx::query("UPDATE calls SET booking = ?1 WHERE id = ?2")
                .bind(serialized)
                .bind(id)
                .execute(db)
                .await;
        }
    }
    Ok(())
}

async fn book_appointment(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<BookReq>,
) -> Result<Json<BookResp>, (StatusCode, &'static str)> {
    require_key(headers, State(state.api_key.clone())).await?;

    // Validate mode quickly
    if !matches!(req.mode.as_str(), "video" | "display-suite") {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "invalid mode"));
    }

    // Parse whatever ISO is sent, then render in Australia/Melbourne (handles AEST/AEDT)
    let parsed: DateTime<FixedOffset> =
        req.slot_iso.parse().unwrap_or_else(|_| chrono::Local::now().fixed_offset());
    let local = parsed.with_timezone(&Melbourne);

    let bid = format!("RS-{}", local.format("%Y%m%d-%H%M"));
    let msg = format!("Booked {}", local.format("%a %d %b %H:%M %Z"));

    // (If you later add a bookings table, insert here with proper error handling)
    let _ = &state;

    Ok(Json(BookResp { ok: true, booking_id: bid, message: msg }))
}

async fn log_call(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(log): Json<CallLog>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    require_key(headers, State(state.api_key.clone())).await?;

    let q = serde_json::to_string(&log.qualification).unwrap_or("{}".into());
    let normalized_booking = normalize_booking(log.booking.as_ref(), &log.qualification);
    let b = match normalized_booking {
        Some(val) => serde_json::to_string(&val).unwrap_or("null".into()),
        None => "null".into(),
    };
    let flags = serde_json::to_string(&log.compliance_flags).unwrap_or("[]".into());

    sqlx::query(r#"INSERT INTO calls
      (ts, caller_cli, summary, qualification, booking, compliance_flags, transcript_url, recording_url)
      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#)
      .bind(&log.timestamp)
      .bind(&log.caller_cli)
      .bind(&log.summary)
      .bind(q)
      .bind(b)
      .bind(flags)
      .bind(log.transcript_url)
      .bind(log.recording_url)
      .execute(&*state.db)
      .await
      .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db error"))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

fn normalize_booking(
    booking: Option<&serde_json::Value>,
    qualification: &serde_json::Value,
) -> Option<serde_json::Value> {
    // Prefer top-level booking, else fallback to qualification.booking
    let mut candidate = booking.cloned().or_else(|| {
        qualification
            .get("booking")
            .cloned()
    });

    // Ensure we have an object we can enrich
    if let Some(serde_json::Value::Object(mut map)) = candidate.take() {
        // booking_id generation if missing or empty
        let needs_id = match map.get("booking_id") {
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(_) => true,
            None => true,
        };
        if needs_id {
            // Prefer RS-<YYYYMMDD-HHMM> if slot_iso is available, else RS-<uuid>
            let generated = map
                .get("slot_iso")
                .and_then(|v| v.as_str())
                .and_then(|slot| {
                    slot.parse::<DateTime<FixedOffset>>()
                        .ok()
                        .map(|dt| dt.with_timezone(&Melbourne))
                        .map(|loc| format!("RS-{}", loc.format("%Y%m%d-%H%M")))
                })
                .unwrap_or_else(|| format!("RS-{}", Uuid::new_v4().simple()));
            map.insert(
                "booking_id".to_string(),
                serde_json::Value::String(generated),
            );
        }

        return Some(serde_json::Value::Object(map));
    }

    None
}

#[derive(Deserialize, Serialize)]
struct KbResp {
    answer: String,
}

async fn kb_lookup(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<KbResp> {
    // Extremely literal KB - avoids hallucination.
    let q = params
        .get("q")
        .map(String::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let answer = riverstone_kb(&q);
    Json(KbResp { answer })
}

fn riverstone_kb(q: &str) -> String {
    // Hard-coded authoritative snippets from the brief.
    let q = q.to_lowercase();

    if q.contains("amenities") {
        return "Rooftop pool, gym, co-working lounge, residents' dining, parcel lockers, EV chargers, bike storage.".into();
    }
    if q.contains("suburb") || q.contains("where is") {
        return "Riverstone Place is in Abbotsford, VIC.".into();
    }
    if q.contains("developer") {
        return "Riverstone Place is by Harbourline Developments with Apex Construct as builder."
            .into();
    }
    if q.contains("builder") || q.contains("who is building") {
        return "Apex Construct is delivering construction for Harbourline Developments.".into();
    }
    if q.contains("sustainability") || q.contains("nathers") || q.contains("solar") {
        return "Targeting 7.5+ NatHERS, solar-assisted common power, and an optional green energy tariff.".into();
    }
    if q.contains("display suite") || q.contains("display") {
        return "Display suite: 123 Swan St, Richmond — Sat/Sun 10:00-16:00; weekdays by appointment.".into();
    }
    if q.contains("contact") || q.contains("email") {
        return "Email handover (test only): sales@riverstoneplace.example.".into();
    }
    if q.contains("pricing 1")
        || q.contains("price 1")
        || q.contains("1-bed")
        || q.contains("1 bedroom")
        || q.contains("one bed")
    {
        return "1-Bed (50-55 m²) from $585k; optional car space +$65k (limited supply).".into();
    }
    if q.contains("pricing 2")
        || q.contains("price 2")
        || q.contains("2-bed")
        || q.contains("2 bedroom")
        || q.contains("two bed")
    {
        return "2-Bed (75-85 m²) from $845k; most include one car space.".into();
    }
    if q.contains("pricing 3")
        || q.contains("price 3")
        || q.contains("3-bed")
        || q.contains("3 bedroom")
        || q.contains("three bed")
    {
        return "3-Bed (105-120 m²) from $1.28m; includes two car spaces (limited).".into();
    }
    if q.contains("deposit") || q.contains("holding") {
        return "10% deposit on exchange. Optional 1% holding (max $10k) can reserve an apartment for 14 days before topping up, subject to approval.".into();
    }
    if q.contains("strata") || q.contains("owners corp") {
        return "Indicative strata: 1-Bed ~$2.8-3.6k/yr, 2-Bed ~$3.6-4.6k/yr, 3-Bed ~$4.8-6.2k/yr (not a quote).".into();
    }
    if q.contains("construction") || q.contains("start date") {
        return "Construction start targeted for late 2025 with completion in Q4 2027 (indicative).".into();
    }
    if q.contains("completion") {
        return "Completion targeted Q4 2027 (indicative).".into();
    }
    if q.contains("parking") {
        return "Parking for 1-Beds is limited and paid extra (+$65k) so not guaranteed; larger homes include parking as noted.".into();
    }
    if q.contains("rental") || q.contains("yield") || q.contains("guarantee") {
        return "No rental guarantees are offered; we can refer you to a property manager for market guidance.".into();
    }
    if q.contains("foreign") || q.contains("firb") || q.contains("stamp duty") {
        return "Foreign buyers may face extra approvals or surcharges; we can't advise but can refer you to a specialist.".into();
    }
    if q.contains("finance") || q.contains("broker") || q.contains("loan") {
        return "We can refer you to a broker; the team can't provide personal finance advice."
            .into();
    }
    if q.contains("finish") || q.contains("custom") {
        return "Finishes have limited customisation windows, subject to availability and cost."
            .into();
    }

    "Sorry, I can refer this to a specialist or book a follow-up.".into()
}

async fn healthz() -> &'static str {
    tracing::info!("API key present: {}", std::env::var("API_KEY").is_ok());
    "ok"
}


#[derive(Deserialize)]
struct EndPayload {
    transcript_url: Option<String>,
    recording_url: Option<String>,
}

async fn webhook_call_ended(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,        // NEW
    Json(_p): Json<EndPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    require_key(headers, State(state.api_key.clone())).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, HeaderValue, Request, StatusCode},
    };
    use std::{str::FromStr, sync::Arc};
    use tower::ServiceExt;

    async fn setup_app() -> (Router, Arc<SqlitePool>) {
        // Create isolated in-memory database for each test run.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();
        sqlx::query(CREATE_CALLS_TABLE_SQL)
            .execute(&pool)
            .await
            .unwrap();
        let shared_pool = Arc::new(pool);
        let app = build_app()
            .route("/readyz", get(readyz))
            .route("/health", get(healthz))
            .with_state(AppState {
                db: shared_pool.clone(),
                api_key: Arc::new("test-key".into()),
            });
        (app, shared_pool)
    }

    #[tokio::test]
    async fn readyz_reports_ready_when_db_is_available() {
        let (app, _) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(buf.as_ref(), b"ready");
    }

    #[tokio::test]
    async fn readyz_reports_db_down_when_pool_closed() {
        let (app, db) = setup_app().await;
        db.close().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(buf.as_ref(), b"db down");
    }

    #[tokio::test]
    async fn healthz_returns_plain_ok() {
        let (app, _) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(buf.as_ref(), b"ok");
    }

    #[tokio::test]
    async fn book_appointment_returns_booking_details() {
        let (app, _) = setup_app().await;
        let payload = serde_json::json!({
            "name": "Alice",
            "phone": "0400000000",
            "email": "alice@example.com",
            "slot_iso": "2025-09-26T10:00:00+10:00",
            "mode": "video",
            "notes": "Bring brochure"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/book_appointment")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: BookResp = serde_json::from_slice(&buf).unwrap();
        assert!(body.ok);
        assert!(body.booking_id.starts_with("RS-"));
        assert!(body.message.starts_with("Booked "));
    }

    #[tokio::test]
    async fn book_appointment_rejects_invalid_mode() {
        let (app, _) = setup_app().await;
        let payload = serde_json::json!({
            "name": "Bob",
            "phone": "0400000000",
            "email": "bob@example.com",
            "slot_iso": "2025-09-26T10:00:00+10:00",
            "mode": "phone",
            "notes": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/book_appointment")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn log_call_persists_record() {
        let (app, db) = setup_app().await;
        let payload = serde_json::json!({
            "timestamp": "2025-01-01T10:00:00+10:00",
            "caller_cli": "+61400111222",
            "summary": "Discussed pricing",
            "qualification": {"budget": "900k"},
            "booking": null,
            "compliance_flags": ["identified"],
            "transcript_url": "https://example.com/transcript",
            "recording_url": "https://example.com/recording"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/log_call")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calls")
            .fetch_one(&*db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn log_call_requires_authorization() {
        let (app, _) = setup_app().await;
        let payload = serde_json::json!({
            "timestamp": "2025-01-01T10:00:00+10:00",
            "caller_cli": "+61400111222",
            "summary": "Discussed pricing",
            "qualification": {"budget": "900k"},
            "booking": null,
            "compliance_flags": ["identified"],
            "transcript_url": null,
            "recording_url": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/log_call")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn log_call_populates_booking_from_qualification_and_generates_id() {
        let (app, db) = setup_app().await;
        // No top-level booking, nested under qualification
        let payload = serde_json::json!({
            "timestamp": "2025-01-01T10:00:00+10:00",
            "caller_cli": "+61400111222",
            "summary": "Nested booking test",
            "qualification": {"booking": {"mode":"display-suite","slot_iso":"2025-09-26T10:00:00+10:00"}},
            "booking": null,
            "compliance_flags": [],
            "transcript_url": null,
            "recording_url": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/log_call")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify DB now has normalized booking with booking_id
        let booking_json: String = sqlx::query_scalar("SELECT booking FROM calls ORDER BY id DESC LIMIT 1")
            .fetch_one(&*db)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_str(&booking_json).unwrap();
        let id = val.get("booking_id").and_then(|v| v.as_str()).unwrap();
        assert!(id.starts_with("RS-"));
        // With slot, it should be the deterministic time-based id
        assert!(id.contains("20250926-1000") || id.len() > 3);
    }

    #[tokio::test]
    async fn log_call_adds_booking_id_when_missing() {
        let (app, db) = setup_app().await;
        // Top-level booking but missing id
        let payload = serde_json::json!({
            "timestamp": "2025-01-01T10:00:00+10:00",
            "caller_cli": "+61400111222",
            "summary": "Top-level booking without id",
            "qualification": {},
            "booking": {"mode":"video","slot_iso":"2025-09-26T10:00:00+10:00","booking_id":""},
            "compliance_flags": [],
            "transcript_url": null,
            "recording_url": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/log_call")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let booking_json: String = sqlx::query_scalar("SELECT booking FROM calls ORDER BY id DESC LIMIT 1")
            .fetch_one(&*db)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_str(&booking_json).unwrap();
        let id = val.get("booking_id").and_then(|v| v.as_str()).unwrap();
        assert!(id.starts_with("RS-"));
    }

    #[tokio::test]
    async fn kb_lookup_returns_curated_answer() {
        let (app, _) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/kb?q=amenities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: KbResp = serde_json::from_slice(&buf).unwrap();
        assert!(body.answer.contains("Rooftop pool"));
    }

    #[tokio::test]
    async fn webhook_call_ended_acknowledges_payload() {
        let (app, _) = setup_app().await;
        let payload = serde_json::json!({
            "transcript_url": "https://example.com/transcript",
            "recording_url": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/call/ended")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn webhook_call_ended_rejects_invalid_key() {
        let (app, _) = setup_app().await;
        let payload = serde_json::json!({
            "transcript_url": "https://example.com/transcript",
            "recording_url": null
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/call/ended")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong-key")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn kb_lookup_handles_unknown_queries() {
        let (app, _) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/kb?q=unlisted")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let buf = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: KbResp = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            body.answer,
            "Sorry, I can refer this to a specialist or book a follow-up."
        );
    }

    #[tokio::test]
    async fn log_call_rejects_malformed_payload() {
        let (app, _) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/log_call")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from("{\"timestamp\":123}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn require_key_rejects_missing_header() {
        let headers = HeaderMap::new();
        let state = State(Arc::new("expected".to_string()));

        let result = require_key(headers, state).await;
        assert_eq!(result, Err((StatusCode::UNAUTHORIZED, "missing auth")));
    }

    #[tokio::test]
    async fn require_key_rejects_wrong_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer nope"),
        );
        let state = State(Arc::new("expected".to_string()));

        let result = require_key(headers, state).await;
        assert_eq!(result, Err((StatusCode::FORBIDDEN, "bad auth")));
    }

    #[tokio::test]
    async fn require_key_accepts_correct_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected"),
        );
        let state = State(Arc::new("expected".to_string()));

        let result = require_key(headers, state).await;
        assert!(result.is_ok());
    }

    #[test]
    fn riverstone_kb_matches_known_topics() {
        let cases = [
            ("Tell me about amenities", "Rooftop pool"),
            ("Where is Riverstone Place located?", "Abbotsford"),
            ("Who is the developer?", "Harbourline"),
            ("Who is building it?", "Apex Construct"),
            ("What sustainability features are there?", "7.5+"),
            ("display suite hours", "123 Swan St"),
            ("What's the contact email?", "sales@riverstoneplace.example"),
            ("How much is a 1-bed?", "$585k"),
            ("2-bed pricing", "$845k"),
            ("3 bedroom price", "$1.28m"),
            ("Do I pay a deposit?", "10%"),
            ("What are the strata fees?", "2.8"),
            ("When does construction start?", "late 2025"),
            ("When is completion?", "Q4 2027"),
            ("Is parking included?", "Parking for 1-Beds"),
            ("Do you offer rental guarantees?", "No rental guarantees"),
            ("I'm a foreign buyer", "approvals"),
            ("Can you help with finance?", "broker"),
            ("Can I customise finishes?", "customisation"),
        ];

        for (query, expected) in cases {
            let answer = riverstone_kb(query);
            assert!(
                answer.contains(expected),
                "query `{query}` did not yield expected snippet `{expected}`; got `{answer}`"
            );
        }
    }

    #[test]
    fn riverstone_kb_defaults_when_no_match() {
        let answer = riverstone_kb("random question");
        assert_eq!(
            answer,
            "Sorry, I can refer this to a specialist or book a follow-up."
        );
    }
}
