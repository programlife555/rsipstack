use super::dialog::DialogInnerRef;
use super::DialogId;
use crate::dialog::{authenticate::handle_client_authenticate, dialog::DialogState};
use crate::rsip_ext::RsipResponseExt;
use crate::transaction::transaction::Transaction;
use crate::Result;
use rsip::prelude::HeadersExt;
use rsip::{Response, SipMessage, StatusCode};
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

#[derive(Clone)]
pub struct ClientInviteDialog {
    pub(super) inner: DialogInnerRef,
}

impl ClientInviteDialog {
    pub fn id(&self) -> DialogId {
        self.inner.id.lock().unwrap().clone()
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.inner.cancel_token
    }

    pub async fn bye(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }
        let request = self
            .inner
            .make_request(rsip::Method::Bye, None, None, None, None, None)?;
        let resp = self.inner.do_request(request).await?;
        self.inner.transition(DialogState::Terminated(
            self.id(),
            resp.map(|r| r.status_code),
        ))?;
        Ok(())
    }

    pub async fn cancel(&self) -> Result<()> {
        let mut cancel_request = self.inner.initial_request.clone();
        cancel_request.method = rsip::Method::Cancel;
        cancel_request
            .cseq_header_mut()?
            .mut_seq(self.inner.get_local_seq())?;
        cancel_request.body = vec![];
        self.inner.do_request(cancel_request).await?;
        Ok(())
    }

    pub async fn reinvite(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }
        todo!()
    }

    pub async fn info(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }

        let request = self
            .inner
            .make_request(rsip::Method::Info, None, None, None, None, None)?;
        self.inner.do_request(request.clone()).await?;
        self.inner
            .transition(DialogState::Info(self.id(), request))?;
        Ok(())
    }

    pub async fn handle(&mut self, mut tx: Transaction) -> Result<()> {
        trace!(
            "handle request: {:?} state:{}",
            tx.original,
            self.inner.state.lock().unwrap()
        );

        let cseq = tx.original.cseq_header()?.seq()?;
        if cseq < self.inner.remote_seq.load(Ordering::Relaxed) {
            info!(
                "received old request remote_seq: {} > {}",
                self.inner.remote_seq.load(Ordering::Relaxed),
                cseq
            );
            tx.reply(rsip::StatusCode::ServerInternalError).await?;
            return Ok(());
        }

        self.inner.remote_seq.store(cseq, Ordering::Relaxed);

        if self.inner.is_confirmed() {
            match tx.original.method {
                rsip::Method::Invite => {}
                rsip::Method::Bye => return self.handle_bye(tx).await,
                rsip::Method::Info => return self.handle_info(tx).await,
                rsip::Method::Options => return self.handle_options(tx).await,
                _ => {
                    info!("invalid request method: {:?}", tx.original.method);
                    tx.reply(rsip::StatusCode::MethodNotAllowed).await?;
                    return Err(crate::Error::DialogError(
                        "invalid request".to_string(),
                        self.id(),
                    ));
                }
            }
        } else {
            info!(
                "received request before confirmed: {:?}",
                tx.original.method
            );
        }
        Ok(())
    }

    async fn handle_bye(&mut self, mut tx: Transaction) -> Result<()> {
        info!("received bye");
        self.inner
            .transition(DialogState::Terminated(self.id(), None))?;
        tx.reply(rsip::StatusCode::OK).await?;
        Ok(())
    }

    async fn handle_info(&mut self, mut tx: Transaction) -> Result<()> {
        info!("received info {}", tx.original.uri);
        self.inner
            .transition(DialogState::Info(self.id(), tx.original.clone()))?;
        tx.reply(rsip::StatusCode::OK).await?;
        Ok(())
    }

    async fn handle_options(&mut self, mut tx: Transaction) -> Result<()> {
        info!("received options {}", tx.original.uri);
        self.inner
            .transition(DialogState::Options(self.id(), tx.original.clone()))?;
        tx.reply(rsip::StatusCode::OK).await?;
        Ok(())
    }

    pub(super) async fn process_invite(
        &self,
        mut tx: Transaction,
    ) -> Result<(DialogId, Option<Response>)> {
        self.inner.transition(DialogState::Calling(self.id()))?;
        let mut auth_sent = false;
        tx.send().await?;
        let mut dialog_id = self.id();
        let mut final_response = None;
        while let Some(msg) = tx.receive().await {
            match msg {
                SipMessage::Request(_) => {}
                SipMessage::Response(resp) => {
                    match resp.status_code {
                        StatusCode::Trying => {
                            self.inner.transition(DialogState::Trying(self.id()))?;
                            continue;
                        }
                        StatusCode::Ringing | StatusCode::SessionProgress => {
                            self.inner.transition(DialogState::Early(self.id(), resp))?;
                            continue;
                        }
                        StatusCode::ProxyAuthenticationRequired | StatusCode::Unauthorized => {
                            if auth_sent {
                                final_response = Some(resp.clone());
                                info!("received {} response after auth sent", resp.status_code);
                                self.inner.transition(DialogState::Terminated(
                                    self.id(),
                                    Some(resp.status_code),
                                ))?;
                                break;
                            }
                            auth_sent = true;
                            if let Some(credential) = &self.inner.credential {
                                tx = handle_client_authenticate(
                                    self.inner.increment_local_seq(),
                                    tx,
                                    resp,
                                    credential,
                                )
                                .await?;
                                tx.send().await?;
                                continue;
                            } else {
                                info!("received 407 response without auth option");
                                self.inner.transition(DialogState::Terminated(
                                    self.id(),
                                    Some(resp.status_code),
                                ))?;
                            }
                            continue;
                        }
                        _ => {}
                    };
                    final_response = Some(resp.clone());
                    match resp.to_header()?.tag()? {
                        Some(tag) => self.inner.update_remote_tag(tag.value())?,
                        None => {}
                    }

                    let branch = match tx
                        .original
                        .via_header()?
                        .params()?
                        .iter()
                        .find(|p| matches!(p, rsip::Param::Branch(_)))
                    {
                        Some(p) => p.clone(),
                        None => {
                            info!("no branch found in via header");
                            return Err(crate::Error::DialogError(
                                "no branch found in via header".to_string(),
                                self.id(),
                            ));
                        }
                    };

                    let ack = self.inner.make_request(
                        rsip::Method::Ack,
                        resp.cseq_header()?.seq().ok(),
                        None,
                        Some(branch),
                        None,
                        None,
                    )?;

                    if let Ok(id) = DialogId::try_from(&ack) {
                        dialog_id = id;
                    }
                    tx.send_ack(ack).await?;
                    match resp.status_code {
                        StatusCode::OK => {
                            self.inner
                                .transition(DialogState::Confirmed(dialog_id.clone()))?;
                        }
                        _ => {
                            let mut reason = format!("{}", resp.status_code);
                            if let Some(reason_phrase) = resp.reason_phrase() {
                                reason = format!("{};{}", reason, reason_phrase);
                            }
                            self.inner.transition(DialogState::Terminated(
                                self.id(),
                                Some(resp.status_code.clone()),
                            ))?;
                            return Err(crate::Error::DialogError(reason, self.id()));
                        }
                    }
                }
            }
        }
        Ok((dialog_id, final_response))
    }
}
