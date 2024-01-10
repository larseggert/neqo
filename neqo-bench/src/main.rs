// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use neqo_crypto::init;
use neqo_transport::{Connection, ConnectionParameters, EmptyConnectionIdGenerator};
use std::{
    cell::RefCell,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    rc::Rc,
    time::Instant,
};

fn main() {
    init();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
    let client = Connection::new_client(
        "",
        &["bench"],
        Rc::new(RefCell::new(EmptyConnectionIdGenerator::default())),
        addr,
        addr,
        ConnectionParameters::default(),
        Instant::now(),
    );
    let server = Connection::new_server(
        &["certs"],
        &["bench"],
        Rc::new(RefCell::new(EmptyConnectionIdGenerator::default())),
        ConnectionParameters::default(),
    );


    
}
