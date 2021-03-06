//! The main service handler
//!
//! Parses incoming messages and delegates them to the appropriate individual message handlers,
//! passing on the results to the [`main_loop`](../main_loop/index.html)

use jsonrpc;
use jsonrpc::{OkResponse, ErrResponse};

use std::io::{Read, Write};
use std::ops::DerefMut;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Sender;
use std::thread::sleep_ms;

use time;
use hyper::server::{Handler, Request, Response};
use rustc_serialize::{json, Decodable};
use rustc_serialize::json::Json;

use rvi::{Message, RVIHandler, Service};

use message::{BackendServices, LocalServices, Notification};
use handler::{NotifyParams, StartParams, ChunkParams, FinishParams};
use handler::{ReportParams, AbortParams, HandleMessageParams, Transfers};
use configuration::Configuration;

/// Type that encodes a single service handler.
///
/// Holds the necessary state, like in-progress transfers, that are needed for handling incoming
/// messages and sending replies to RVI. Needs to be thread safe as
/// [`hyper`](../../../hyper/index.html) handles requests asynchronously.
pub struct ServiceHandler {
    /// The full URL, where RVI can be reached.
    rvi_url: String,
    /// A `Sender` that connects the handlers with the `main_loop`.
    sender: Mutex<Sender<Notification>>,
    /// The service URLs that the SOTA server advertised.
    services: Mutex<BackendServices>,
    /// The currently in-progress `Transfer`s.
    transfers: Arc<Mutex<Transfers>>,
    /// The full `Configuration` of sota_client.
    conf: Configuration,
    /// The VIN of this device, as returned by RVI.
    vin: String
}

impl ServiceHandler {
    /// Create a new `ServiceHandler`.
    ///
    /// # Arguments
    /// * `transfers`: A `Transfers` object to store the in-progress `Transfer`s.
    /// * `sender`: A `Sender` to call back into the `main_loop`.
    /// * `url`: The full URL, where RVI can be reached.
    /// * `c`: The full `Configuration` of sota_client.
    pub fn new(transfers: Arc<Mutex<Transfers>>,
               sender: Sender<Notification>,
               url: String, c: Configuration) -> ServiceHandler {
        let services = BackendServices {
            start: String::new(),
            ack: String::new(),
            report: String::new(),
            packages: String::new()
        };

        ServiceHandler {
            rvi_url: url,
            sender: Mutex::new(sender),
            services: Mutex::new(services),
            transfers: transfers,
            vin: String::new(),
            conf: c
        }
    }

    /// Starts a infinite loop to expire timed out transfers. Checks once a second for timed out
    /// transfers.
    ///
    /// # Arguments
    /// * `transfers`: Pointer to a `Transfers` object, that stores the transfers to be checked for
    ///   expired timeouts.
    /// * `timeout`: The timeout in seconds.
    pub fn start_timer(transfers: &Mutex<Transfers>,
                       timeout: i64) {
        loop {
            sleep_ms(1000);
            let time_now = time::get_time().sec;
            let mut transfers = transfers.lock().unwrap();

            let mut timed_out = Vec::new();
            for transfer in transfers.deref_mut() {
                if time_now - transfer.1.last_chunk_received > timeout {
                    timed_out.push(transfer.0.clone());
                }
            }

            for transfer in timed_out {
                info!("Transfer for package {} timed out after {} ms",
                      transfer, timeout);
                let _ = transfers.remove(&transfer);
            }
        }
    }

    /// Helper function to send a `Notification` to the `main_loop`.
    ///
    /// # Arguments
    /// * `m`: `Notification` to send.
    fn push_notify(&self, m: Notification) {
        try_or!(self.sender.lock().unwrap().send(m), return);
    }

    /// Create a message handler `D`, and let it process the `message`. If it returns a
    /// Notification, forward it to the `main_loop`. Returns a `jsonrpc` response indicating
    /// success or failure.
    ///
    /// # Arguments
    /// * `message`: The message, that should be handled.
    fn handle_message_params<D>(&self, message: &str)
        -> Option<Result<OkResponse<i32>, ErrResponse>>
        where D: Decodable + HandleMessageParams {
        json::decode::<jsonrpc::Request<Message<D>>>(&message).map(|p| {
            let handler = &p.params.parameters[0];
            let result = handler.handle(&self.services,
                                        &self.transfers,
                                        &self.rvi_url,
                                        &self.vin,
                                        &self.conf.client.storage_dir);
            if result {
                handler.get_message().map(|m| { self.push_notify(m); });
                Ok(OkResponse::new(p.id, None))
            } else {
                Err(ErrResponse::unspecified(p.id))
            }
        }).ok()
    }

    /// Try to parse the type of a message and forward it to the appropriate message handler.
    /// Returns the result of the message handling or a `jsonrpc` result indicating a parser error.
    ///
    /// Needs to be extended to support new services.
    ///
    /// # Arguments
    /// * `message`: The message that will be parsed.
    fn handle_message(&self, message: &str)
        -> Result<OkResponse<i32>, ErrResponse> {
        macro_rules! handle_params {
            ($handler:ident, $message:ident, $service:ident, $id:ident,
             $( $x:ty, $i:expr), *) => {{
                $(
                    if $i == $service {
                        match $handler.handle_message_params::<$x>($message) {
                            Some(r) => return r,
                            None => return Err(ErrResponse::invalid_params($id))
                        }
                    }
                )*
            }}
        }

        let data = try!(Json::from_str(message)
                        .map_err(|_| ErrResponse::parse_error()));
        let obj = try!(data.as_object().ok_or(ErrResponse::parse_error()));
        let rpc_id = try!(obj.get("id").and_then(|x| x.as_u64())
                          .ok_or(ErrResponse::parse_error()));

        let method = try!(obj.get("method").and_then(|x| x.as_string())
                          .ok_or(ErrResponse::invalid_request(rpc_id)));

        if method == "services_available" {
            Ok(OkResponse::new(rpc_id, None))
        }
        else if method != "message" {
            Err(ErrResponse::method_not_found(rpc_id))
        } else {
            let service = try!(obj.get("params")
                               .and_then(|x| x.as_object())
                               .and_then(|x| x.get("service_name"))
                               .and_then(|x| x.as_string())
                               .ok_or(ErrResponse::invalid_request(rpc_id)));

            handle_params!(self, message, service, rpc_id,
                           NotifyParams, "/sota/notify",
                           StartParams,  "/sota/start",
                           ChunkParams,  "/sota/chunk",
                           FinishParams, "/sota/finish",
                           ReportParams, "/sota/getpackages",
                           AbortParams,  "/sota/abort");

            Err(ErrResponse::invalid_request(rpc_id))
        }
    }
}

impl Handler for ServiceHandler {
    fn handle(&self, mut req: Request, resp: Response) {
        let mut rbody = String::new();
        try_or!(req.read_to_string(&mut rbody), return);
        debug!(">>> Received Message: {}", rbody);
        let mut resp = try_or!(resp.start(), return);

        macro_rules! send_response {
            ($rtype:ty, $resp:ident) => {
                match json::encode::<$rtype>(&$resp) {
                    Ok(decoded_msg) => {
                        try_or!(resp.write_all(decoded_msg.as_bytes()), return);
                        debug!("<<< Sent Response: {}", decoded_msg);
                    },
                    Err(p) => { error!("{}", p); }
                }
            };
        }

        match self.handle_message(&rbody) {
            Ok(msg) => { send_response!(OkResponse<i32>, msg) },
            Err(msg) => { send_response!(ErrResponse, msg) }
        }

        try_or!(resp.end(), return);
    }
}

impl RVIHandler for ServiceHandler {
    fn register(&mut self, services: Vec<Service>) {
        self.vin = LocalServices::new(&services)
            .get_vin(self.conf.client.vin_match);
    }
}
