//! Session chat enqueue validation. Passive watches belong to the daemon client.
use super::*;

impl DaemonClient {
    pub(super) async fn send_session_chat_or_session(
        &self,
        request: Value,
        generation: Option<u64>,
    ) -> Result<Value, String> {
        if let Some(expected) = generation {
            if self.session.current_generation().await != expected {
                return Err("session was recreated".into());
            }
        }
        let session_chat = request["op"] == "chat" && request.get("target").is_none();
        if !session_chat {
            return match generation {
                Some(generation) => {
                    self.session
                        .send_on_session_for_generation(request, generation)
                        .await
                }
                None => self.session.send_on_session(request).await,
            };
        }
        let (expected_generation, session_id, id) = self.chat_identity().await;
        if generation.is_some_and(|expected| expected != expected_generation) {
            return Err("session was recreated".into());
        }
        let hook = self.chat_watch.lock().unwrap().upgrade();
        if let Some(hook) = &hook {
            // Reservation watches are best effort: enqueue can create or replace
            // the run, then confirmed() opens replay for any missed early output.
            if let Err(error) = hook
                .ensure(self, expected_generation, &session_id, &id)
                .await
            {
                if error == crate::daemon_client::chat::RETIRED {
                    return Err(error);
                }
            }
        }
        let turn = request["turn"].as_str().map(str::to_owned);
        let response = self
            .session
            .send_on_session_for_generation(request, expected_generation)
            .await?;
        if response["type"] == "error" {
            return Ok(response);
        }
        if let Some(hook) = hook {
            hook.confirmed(self, expected_generation, &session_id, &response, turn)
                .await?;
        } else {
            self.confirm_chat_identity(expected_generation, &session_id, &response, turn)
                .await?;
        }
        let mut response = response;
        if let Some(object) = response.as_object_mut() {
            object.remove(REPLY_ORDER_FIELD);
        }
        Ok(response)
    }

    pub(crate) async fn confirm_chat_identity(
        &self,
        expected_generation: u64,
        session_id: &str,
        response: &Value,
        turn: Option<String>,
    ) -> Result<String, String> {
        let mut write = self.session.write.lock().await;
        if write.retired
            || write.generation != expected_generation
            || write.session_id.as_deref() != Some(session_id)
            || response["session_id"]
                .as_str()
                .is_some_and(|id| id != session_id)
        {
            return Err("session_chat acknowledgment belongs to a retired session".into());
        }
        if response["type"] != "ok" || response["enqueued"] != true {
            return Err("unexpected session_chat enqueue acknowledgment".into());
        }
        let actual = response["chat_run_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "session_chat enqueue acknowledgment has no run id".to_owned())?
            .to_owned();
        let order = response[REPLY_ORDER_FIELD].as_u64().unwrap_or(0);
        if order != 0 && order < write.confirmation_order {
            return Err("session_chat acknowledgment superseded by a newer reply".into());
        }
        write.confirmation_order = order;
        write.chat_run_id = Some(actual.clone());
        let _ = self.out.raw_tx.send(json!({
            "type":"session_chat_run_identity", "id":actual,
            "turn":turn, "session_id":session_id, "generation":expected_generation
        }));
        Ok(actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn older_delivered_reply_cannot_publish_after_newer_confirmation() {
        let (client, _requests, _events, _chat, mut raw) = DaemonClient::fake_with_requests(
            json!({"type":"ok", "session_id":"owner", "chat_run_id":"reserved"}),
        );
        let (older_tx, older_rx) = oneshot::channel();
        let (newer_tx, newer_rx) = oneshot::channel();
        FakeReply(older_tx)
            .send(Ok(json!({"type":"ok", "enqueued":true,
            "session_id":"owner", "chat_run_id":"older"})))
            .unwrap();
        FakeReply(newer_tx)
            .send(Ok(json!({"type":"ok", "enqueued":true,
            "session_id":"owner", "chat_run_id":"newer"})))
            .unwrap();
        let older = older_rx.await.unwrap().unwrap();
        let newer = newer_rx.await.unwrap().unwrap();
        client
            .confirm_chat_identity(0, "owner", &newer, None)
            .await
            .unwrap();
        assert!(client
            .confirm_chat_identity(0, "owner", &older, None)
            .await
            .unwrap_err()
            .contains("superseded"));
        assert_eq!(client.chat_run_id().await, "newer");
        assert_eq!(raw.try_recv().unwrap()["id"], "newer");
        assert!(raw.try_recv().is_err());
        client.shutdown().await;
    }
}
