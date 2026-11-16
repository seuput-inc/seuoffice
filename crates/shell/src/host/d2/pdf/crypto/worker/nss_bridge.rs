//! Linux NSS helper v2 pipe transport. Only a verified bundle may spawn
//! the owned child passed here; this module never accepts executable paths.
use super::nss_bundle::NssChild;
use super::{Channel, wire::Kind};
use zeroize::Zeroizing;

/// The caller supplies the verified helper child in the signer's process group:
/// cancellation of the signing worker terminates the complete group. NssChild
/// owns cleanup and reaps the process on every normal/error return.
pub(super) fn export(
    child: NssChild,
    dir: &str,
    nickname: &str,
    password: &str,
    channel: &mut Channel,
) -> Result<(Zeroizing<Vec<u8>>, Zeroizing<String>), String> {
    super::nss_exchange::exchange(child, dir, nickname, password, |name| {
        channel.send(Kind::ConfirmToken, name.as_bytes())?;
        let (reply, nonce) = super::wire::read_frame(&mut channel.input, channel.nonce.len())?;
        if nonce.as_slice() != channel.nonce {
            return Err("NSS helper protocol failure".into());
        }
        match reply {
            Kind::ConfirmAccept => Ok(true),
            Kind::ConfirmDeny => Ok(false),
            _ => Err("NSS helper protocol failure".into()),
        }
    })
}
