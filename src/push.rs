use axum::http::StatusCode;
use base64::URL_SAFE_NO_PAD;
use tracing::error;
use web_push::{
    ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushError, WebPushMessageBuilder,
};

use crate::{config::Config, db::db_delete, error::AppError, models::PushSubscription};
use redb::Database;

pub async fn send_push(
    cfg: &Config,
    db: &Database,
    push_client: &web_push::WebPushClient,
    uuid: &str,
    subscription: &PushSubscription,
    payload: &[u8],
) -> Result<(), AppError> {
    // Web Push requires endpoint + p256dh + auth (from browser subscription).
    let subscription_info = SubscriptionInfo::new(
        subscription.endpoint.clone(),
        subscription.keys.p256dh.clone(),
        subscription.keys.auth.clone(),
    );

    let mut builder =
        WebPushMessageBuilder::new(&subscription_info).map_err(|err| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("push builder error: {err}"),
            )
        })?;

    // Encrypt payload per RFC 8030 (AES-128-GCM).
    builder.set_payload(ContentEncoding::Aes128Gcm, payload);
    builder.set_ttl(60);

    // Sign VAPID JWT (ES256) so push services can authenticate the sender.
    let mut vapid_builder = VapidSignatureBuilder::from_base64(
        &cfg.vapid_private_key,
        URL_SAFE_NO_PAD,
        &subscription_info,
    )
    .map_err(|err| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    vapid_builder.add_claim("sub", cfg.vapid_subject.as_str());
    let signature = vapid_builder
        .build()
        .map_err(|err| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    builder.set_vapid_signature(signature);

    let message = match builder.build() {
        Ok(message) => message,
        Err(WebPushError::PayloadTooLarge) => {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "push payload too large",
            ))
        }
        Err(err) => {
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                err.to_string(),
            ))
        }
    };

    match push_client.send(message).await {
        Ok(()) => Ok(()),
        Err(WebPushError::EndpointNotValid) | Err(WebPushError::EndpointNotFound) => {
            // Remove dead subscriptions when push services report expiration.
            let _ = db_delete(db, uuid);
            error!("subscription expired for {uuid}");
            Err(AppError::new(
                StatusCode::BAD_GATEWAY,
                "subscription expired",
            ))
        }
        Err(WebPushError::PayloadTooLarge) => Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "push payload too large",
        )),
        Err(err) => {
            error!("push failed: {err}");
            Err(AppError::new(
                StatusCode::BAD_GATEWAY,
                format!("push failed: {err}"),
            ))
        }
    }

}
