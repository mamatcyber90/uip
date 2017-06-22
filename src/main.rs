#![feature(conservative_impl_trait)]

extern crate interfaces;
extern crate igd;
extern crate rustls;
extern crate futures;
extern crate tokio_io;
extern crate tokio_core;
extern crate tokio_rustls;
extern crate tokio_file_unix;
extern crate webpki_roots;

mod connection;
mod state;
use state::{State};

use rustls::{Certificate};
use rustls::internal::pemfile::{ certs };
use std::net::ToSocketAddrs;
use std::io::{ BufReader };

fn load_certs(path: &str) -> Vec<Certificate> {
    certs(&mut BufReader::new(std::fs::File::open(path).unwrap())).unwrap()
}

fn main() {
    let addr = "127.0.0.1:4433".to_socket_addrs().unwrap().next().unwrap();
    let cert = load_certs("rsa/ca.cert").pop().unwrap();
    let state = State::new("test".to_string());
    state.add_relay("testserver.com".to_string());
    state.add_relay_peer("testserver.com".to_string(), addr, cert.clone());
    state.run();
}

