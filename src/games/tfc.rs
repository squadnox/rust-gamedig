use std::net::Ipv4Addr;
use crate::{
    protocols::valve::{self, game, SteamApp},
    GDResult,
};

pub fn query(address: &Ipv4Addr, port: Option<u16>) -> GDResult<game::Response> {
    let valve_response = valve::query(
        address,
        port.unwrap_or(27015),
        SteamApp::TFC.as_engine(),
        None,
        None,
    )?;

    Ok(game::Response::new_from_valve_response(valve_response))
}
