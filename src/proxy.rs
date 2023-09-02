use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use openssl::ssl::{Ssl, SslConnector};
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_openssl::SslStream;

use ldap3_proto::proto::*;
use ldap3_proto::LdapCodec;

use crate::{AppState, DnConfig};

type CR = ReadHalf<SslStream<TcpStream>>;
type CW = WriteHalf<SslStream<TcpStream>>;

enum ClientState {
    Unbound,
    Authenticated {
        dn: String,
        config: DnConfig,
        client: BasicLdapClient,
    },
}

fn bind_operror(msgid: i32, msg: &str) -> LdapMsg {
    LdapMsg {
        msgid: msgid,
        op: LdapOp::BindResponse(LdapBindResponse {
            res: LdapResult {
                code: LdapResultCode::OperationsError,
                matcheddn: "".to_string(),
                message: msg.to_string(),
                referral: vec![],
            },
            saslcreds: None,
        }),
        ctrl: vec![],
    }
}

pub(crate) async fn client_process<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    mut r: FramedRead<R, LdapCodec>,
    mut w: FramedWrite<W, LdapCodec>,
    client_address: SocketAddr,
    app_state: Arc<AppState>,
) {
    debug!("Accept from {}", client_address);

    // We always start unbound.
    let mut state = ClientState::Unbound;

    // Start to wait for incomming packets
    while let Some(Ok(protomsg)) = r.next().await {
        let next_state = match (&state, protomsg) {
            // Doesn't matter what state we are in, any bind will trigger this process.
            (
                _,
                LdapMsg {
                    msgid,
                    op: LdapOp::BindRequest(lbr),
                    ctrl,
                },
            ) => {
                trace!(?lbr);
                // Is the requested bind dn valid per our map?
                let config = match app_state.binddn_map.get(&lbr.dn) {
                    Some(dnconfig) => {
                        // They have a config! They can proceed.
                        dnconfig.clone()
                    }
                    None => {
                        // No config found, sad trombone.
                        let resp_msg = bind_operror(msgid, "unable to bind");
                        if w.send(resp_msg).await.is_err() {
                            error!("Unable to send response");
                            break;
                        }
                        continue;
                    }
                };

                // Okay, we have a dnconfig, so they are allowed to proceed. Lets
                // now setup the client for their session, and anything else we
                // need to configure.

                let dn = lbr.dn.clone();

                // We need the client to connect *and* bind to proceed here!
                let mut client =
                    match BasicLdapClient::build(&app_state.addrs, &app_state.tls_params).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!(?e, "A client build error has occured.");
                            let resp_msg = bind_operror(msgid, "unable to bind");
                            if w.send(resp_msg).await.is_err() {
                                error!("Unable to send response");
                            }
                            // Always bail.
                            break;
                        }
                    };

                let valid = match client.bind(lbr, ctrl).await {
                    Ok((bind_resp, ctrl)) => {
                        // Almost there, lets check the bind result.
                        let valid = bind_resp.res.code == LdapResultCode::Success;

                        let resp_msg = LdapMsg {
                            msgid,
                            op: LdapOp::BindResponse(bind_resp),
                            ctrl,
                        };
                        if w.send(resp_msg).await.is_err() {
                            error!("Unable to send response");
                            break;
                        }
                        valid
                    }
                    Err(e) => {
                        error!(?e, "A client bind error has occured");
                        let resp_msg = bind_operror(msgid, "unable to bind");
                        if w.send(resp_msg).await.is_err() {
                            error!("Unable to send response");
                        }
                        // Always bail.
                        break;
                    }
                };

                if valid {
                    Some(ClientState::Authenticated { dn, config, client })
                } else {
                    None
                }
            }
            // Unbinds are always actioned.
            (
                _,
                LdapMsg {
                    msgid,
                    op: LdapOp::UnbindRequest,
                    ctrl: _,
                },
            ) => {
                trace!("unbind");
                break;
            }

            // Authenticated message handler.
            //  - Search
            (
                ClientState::Authenticated {
                    dn,
                    config,
                    ref mut client,
                },
                LdapMsg {
                    msgid,
                    op: LdapOp::SearchRequest(sr),
                    ctrl,
                },
            ) => {
                // Pre check if the search is allowed for this dn / filter

                // If not, send and empty result.

                // If yes, continue.
            }
            //  - Whoami

            // Unknown message handler.
            (_, msg) => {
                debug!(?msg);
                // Return a disconnect.

                todo!();
            }
        };

        if let Some(next_state) = next_state {
            // Update the client state, dropping any former state.
            state = next_state;
        }
    }
    debug!("Disconnect for {}", client_address);
}

#[derive(Debug, Clone)]
enum LdapError {
    TlsError,
    ConnectError,
    Transport,
    InvalidProtocolState,
}

struct BasicLdapClient {
    r: FramedRead<CR, LdapCodec>,
    w: FramedWrite<CW, LdapCodec>,
    msg_counter: i32,
}

impl BasicLdapClient {
    fn next_msgid(&mut self) -> i32 {
        self.msg_counter += 1;
        self.msg_counter
    }

    pub async fn build(
        addrs: &[SocketAddr],
        tls_connector: &SslConnector,
    ) -> Result<Self, LdapError> {
        let timeout = Duration::from_secs(5);

        let mut aiter = addrs.iter();

        let tcpstream = loop {
            if let Some(addr) = aiter.next() {
                let sleep = tokio::time::sleep(timeout);
                tokio::pin!(sleep);
                tokio::select! {
                    maybe_stream = TcpStream::connect(addr) => {
                        match maybe_stream {
                            Ok(t) => {
                                trace!(?addr, "connection established");
                                break t;
                            }
                            Err(e) => {
                                trace!(?addr, ?e, "error");
                                continue;
                            }
                        }
                    }
                    _ = &mut sleep => {
                        warn!(?addr, "timeout");
                        continue;
                    }
                }
            } else {
                return Err(LdapError::ConnectError);
            }
        };

        let mut tlsstream = Ssl::new(tls_connector.context())
            .and_then(|tls_obj| SslStream::new(tls_obj, tcpstream))
            .map_err(|e| {
                error!(?e, "openssl");
                LdapError::TlsError
            })?;

        let _ = SslStream::connect(Pin::new(&mut tlsstream))
            .await
            .map_err(|e| {
                error!(?e, "openssl");
                LdapError::TlsError
            })?;

        info!("tls configured");
        let (r, w) = tokio::io::split(tlsstream);

        let w = FramedWrite::new(w, LdapCodec);
        let r = FramedRead::new(r, LdapCodec);

        Ok(BasicLdapClient {
            r,
            w,
            msg_counter: 0,
        })
    }

    pub async fn bind(
        &mut self,
        lbr: LdapBindRequest,
        ctrl: Vec<LdapControl>,
    ) -> Result<(LdapBindResponse, Vec<LdapControl>), LdapError> {
        let msgid = self.next_msgid();

        let msg = LdapMsg {
            msgid,
            op: LdapOp::BindRequest(lbr),
            ctrl,
        };

        self.w.send(msg).await.map_err(|e| {
            error!(?e, "unable to transmit to ldap server");
            LdapError::Transport
        })?;

        match self.r.next().await {
            Some(Ok(LdapMsg {
                msgid,
                op: LdapOp::BindResponse(bind_resp),
                ctrl,
            })) => Ok((bind_resp, ctrl)),
            Some(Ok(msg)) => {
                trace!(?msg);
                Err(LdapError::InvalidProtocolState)
            }
            Some(Err(e)) => {
                error!(?e, "unable to receive from ldap server");
                Err(LdapError::Transport)
            }
            None => {
                error!("connection closed");
                Err(LdapError::Transport)
            }
        }
    }
}
