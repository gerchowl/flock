//! `channel.hello` (ADR-0028 §8, #1037): a channel adapter proves it holds
//! a binding's capability token before it serves the binding.

use super::responses::{encode_error, encode_success};
use crate::api::schema::{ChannelHelloParams, ResponseResult};
use crate::app::App;
use crate::channels::HelloRefusal;

impl App {
    pub(super) fn handle_channel_hello(
        &mut self,
        id: String,
        params: ChannelHelloParams,
    ) -> String {
        let node = crate::app::short_host_name();
        match crate::channels::hello(
            &self.state.config.channels,
            &node,
            &params.binding,
            &params.token,
        ) {
            Ok(binding) => encode_success(
                id,
                ResponseResult::ChannelHello {
                    binding: binding.name.clone(),
                    owner_node: binding.owner_node.clone(),
                    target: binding.target.clone(),
                    scopes: binding.scopes.clone(),
                },
            ),
            Err(HelloRefusal::Unauthorized) => encode_error(
                id,
                "channel_unauthorized",
                format!(
                    "binding {:?} is not configured here or the token does not match its issued token",
                    params.binding
                ),
            ),
            Err(HelloRefusal::NotOwner { owner }) => encode_error(
                id,
                "channel_not_owner",
                format!(
                    "binding {:?} is served by node {owner:?}, not {node:?}; run the adapter there",
                    params.binding
                ),
            ),
        }
    }
}
