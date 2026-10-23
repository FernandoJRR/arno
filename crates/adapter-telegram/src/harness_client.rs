//! Thin HTTP client for Contract 1 (SPEC §4.1) — `POST /v1/orders` and
//! `GET /v1/ready`, bearer-authenticated with this adapter's own client token.

use contract::{ErrorBody, ErrorCode, Order, Readiness, Response};

pub struct HarnessClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

/// The harness's own error surface (`ErrorCode`) is returned as data, not an
/// `Err` variant: it's an expected outcome the caller renders to the user,
/// distinct from a transport failure.
pub enum OrderOutcome {
    Ok(Box<Response>),
    Rejected(ErrorCode),
}

impl HarnessClient {
    pub fn new(http: reqwest::Client, base_url: String, token: String) -> Self {
        Self {
            http,
            base_url,
            token,
        }
    }

    pub async fn send_order(&self, order: &Order) -> Result<OrderOutcome, reqwest::Error> {
        let resp = self
            .http
            .post(format!("{}/v1/orders", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.token)
            .json(order)
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(OrderOutcome::Ok(Box::new(resp.json::<Response>().await?)))
        } else {
            Ok(OrderOutcome::Rejected(
                resp.json::<ErrorBody>().await?.error_code,
            ))
        }
    }

    /// The harness's readiness report (SPEC §11 #31), sent with 200 or 503;
    /// None when it cannot be read.
    pub async fn readiness(&self) -> Option<Readiness> {
        let resp = self
            .http
            .get(format!("{}/v1/ready", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.token)
            .send()
            .await
            .ok()?;
        resp.json::<Readiness>().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::Response;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn order() -> Order {
        Order {
            session_id: "42".into(),
            text: "hi".into(),
            client_msg_id: Some("1".into()),
            confirmation_token: None,
            attachments: Vec::new(),
            decision_answer: None,
        }
    }

    #[tokio::test]
    async fn posts_to_v1_orders_with_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/orders"))
            .and(header("authorization", "Bearer secret-tok"))
            .and(body_json(order()))
            .respond_with(ResponseTemplate::new(200).set_body_json(Response::text("hello back")))
            .mount(&server)
            .await;

        let client = HarnessClient::new(reqwest::Client::new(), server.uri(), "secret-tok".into());
        let outcome = client.send_order(&order()).await.unwrap();
        match outcome {
            OrderOutcome::Ok(resp) => assert_eq!(resp.text, "hello back"),
            OrderOutcome::Rejected(code) => panic!("unexpected rejection: {code:?}"),
        }
    }

    #[tokio::test]
    async fn non_2xx_is_surfaced_as_rejected_error_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/orders"))
            .respond_with(ResponseTemplate::new(409).set_body_json(ErrorBody {
                error_code: ErrorCode::DuplicateOrder,
            }))
            .mount(&server)
            .await;

        let client = HarnessClient::new(reqwest::Client::new(), server.uri(), "tok".into());
        let outcome = client.send_order(&order()).await.unwrap();
        match outcome {
            OrderOutcome::Rejected(code) => assert_eq!(code, ErrorCode::DuplicateOrder),
            OrderOutcome::Ok(_) => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn readiness_is_read_from_200_and_503_and_garbage_is_none() {
        let body = serde_json::json!({"status": "degraded", "issues": ["model_on_cpu"],
            "model": {"name": "m", "reachable": true, "gpu_share": 0.0}});
        for status in [200, 503] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/ready"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(ResponseTemplate::new(status).set_body_json(&body))
                .mount(&server)
                .await;
            let client = HarnessClient::new(reqwest::Client::new(), server.uri(), "tok".into());
            let report = client.readiness().await.expect("parsed");
            assert_eq!(report.issues, ["model_on_cpu"]);
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/ready"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let client = HarnessClient::new(reqwest::Client::new(), server.uri(), "tok".into());
        assert!(client.readiness().await.is_none());
    }
}
