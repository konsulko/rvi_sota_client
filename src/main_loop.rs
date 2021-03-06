//! Main loop, starting the worker threads and wiring up communication channels between them.

use std::sync::mpsc::channel;
use std::thread;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::ops::Deref;

use rvi;
use handler::ServiceHandler;
use message::{InitiateParams, BackendServices, PackageId};
use message::{Notification, ServerPackageReport, LocalServices, ServerReport};
use configuration::Configuration;
use persistence::Transfer;
use sota_dbus;

/// Main loop, starting the worker threads and wiring up communication channels between them.
///
/// # Arguments
/// * `conf`: A pointer to a `Configuration` object see the [documentation of the configuration
///   crate](../configuration/index.html).
/// * `rvi_url`: The URL, where RVI can be found, with the protocol.
/// * `edge_url`: The `host:port` combination where the client should bind and listen for incoming
///   RVI calls.
pub fn start(conf: &Configuration, rvi_url: String, edge_url: String) {
    // will receive RVI registration details
    let (tx_edge, rx_edge) = channel();
    let rvi_edge = rvi::ServiceEdge::new(rvi_url.clone(),
                                         edge_url.clone(),
                                         tx_edge);

    // Holds metadata about running transfers
    let transfers: Arc<Mutex<HashMap<PackageId, Transfer>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // will receive notifies from RVI and install requests from dbus
    let (tx_main, rx_main) = channel();
    let handler = ServiceHandler::new(transfers.clone(), tx_main.clone(),
                                      rvi_url.clone(), conf.clone());

    match conf.client.timeout {
        Some(timeout) => {
            let _ = thread::spawn(move || {
                ServiceHandler::start_timer(transfers.deref(), timeout);
            });
        },
        None => info!("No timeout configured, transfers will never time out.")
    }

    // these services will be registered with RVI. Keep in mind that you also have to write a
    // handler and forward messages to it, when introducing a new service.
    let services = vec!("/sota/notify",
                        "/sota/start",
                        "/sota/chunk",
                        "/sota/finish",
                        "/sota/getpackages",
                        "/sota/abort");

    thread::spawn(move || {
        rvi_edge.start(handler, services);
    });

    let dbus_receiver = sota_dbus::Receiver::new(conf.dbus.clone(),
                                                 tx_main.clone());
    thread::spawn(move || {
        dbus_receiver.start();
    });

    let local_services = LocalServices::new(&rx_edge.recv().unwrap());
    let mut backend_services = BackendServices::new();

    loop {
        match rx_main.recv().unwrap() {
            // Pass on notifications to the DBus
            Notification::Notify(notify) => {
                backend_services.update(&notify.services);
                sota_dbus::send_notify(&conf.dbus, notify.packages);
            },
            // Pass on initiate requests to RVI
            Notification::Initiate(packages) => {
                let initiate =
                    InitiateParams::new(packages, local_services.clone(),
                                        local_services
                                        .get_vin(conf.client.vin_match));
                match rvi::send_message(&rvi_url, initiate,
                                        &backend_services.start) {
                    Ok(..) => {},
                    Err(e) => error!("Couldn't initiate download: {}", e)
                }
            },
            // Request and forward the installation report from DBus to RVI.
            Notification::Finish(package) => {
                let report = sota_dbus::request_install(&conf.dbus, package);
                let server_report =
                    ServerPackageReport::new(report, local_services
                                             .get_vin(conf.client.vin_match));

                match rvi::send_message(&rvi_url, server_report,
                                        &backend_services.report) {
                    Ok(..) => {},
                    Err(e) => error!("Couldn't send report: {}", e)
                }
            },
            // Request a full report via DBus and forward it to RVI
            Notification::Report => {
                let packages = sota_dbus::request_report(&conf.dbus);
                let report =
                    ServerReport::new(packages, local_services
                                      .get_vin(conf.client.vin_match));

                match rvi::send_message(&rvi_url, report,
                                        &backend_services.packages) {
                    Ok(..) => {},
                    Err(e) => error!("Couldn't send report: {}", e)
                }
            }
        }
    }
}
