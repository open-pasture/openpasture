//! Provisioning payloads from what the server hands out when collars are
//! linked: a bulk-link response (`POST /api/collars/bulk`), a single link
//! (`POST /api/collars`, `rekey`), a fleet claim's `v1` credentials
//! (`POST /api/fleet/claim`), or a card payload as printed in the QR.

use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

/// One collar's provisioning, as the firmware's `provision` command takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub collar_id: String,
    pub herd_id: String,
    pub key: String,
    /// Where the collar itself will report once it has a modem: `https://…/collar/v1`.
    pub endpoint: String,
    /// The server's Ed25519 public key, base64.
    pub server_key: String,
    pub name: Option<String>,
    pub tag: Option<String>,
    /// The server's reason when it gave no endpoint.
    pub no_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct Linked {
    collar: LinkedCollar,
    key: String,
    /// Absent until the server has a collar address (`no_endpoint` says why).
    endpoint: Option<String>,
    no_endpoint: Option<String>,
    public_key: String,
    tag: Option<String>,
}

#[derive(Deserialize)]
struct LinkedCollar {
    id: String,
    herd_id: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ClaimV1 {
    key: String,
    public_key: String,
    endpoint: Option<String>,
    why: Option<String>,
}

#[derive(Deserialize)]
struct Card {
    c: String,
    h: String,
    k: String,
    e: String,
    s: String,
}

impl Link {
    /// From a file holding any of the three shapes. `pick` chooses one
    /// collar of a bulk response by id, name or tag.
    pub fn load(path: &Path, pick: Option<&str>) -> anyhow::Result<Link> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text, pick).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str, pick: Option<&str>) -> anyhow::Result<Link> {
        let v: Value = serde_json::from_str(text.trim()).context("not JSON")?;
        if v.get("v").is_some() {
            let c: Card = serde_json::from_value(v).context("a card payload needs v, c, h, k, e and s")?;
            return Ok(Link { collar_id: c.c, herd_id: c.h, key: c.k, endpoint: c.e, server_key: c.s, name: None, tag: None, no_endpoint: None });
        }
        // A claim (`POST /api/fleet/claim`): the v1 credentials of the collar it made.
        if let (Some(v1), Some(collar)) = (v.get("v1"), v.get("collar")) {
            let c: LinkedCollar = serde_json::from_value(collar.clone()).context("the claim's collar")?;
            let v1: ClaimV1 = serde_json::from_value(v1.clone()).context("the claim's v1 credentials")?;
            return Ok(Link {
                collar_id: c.id,
                herd_id: c.herd_id,
                key: v1.key,
                endpoint: v1.endpoint.unwrap_or_default(),
                server_key: v1.public_key,
                name: c.name,
                tag: None,
                no_endpoint: v1.why,
            });
        }
        let all: Vec<Linked> = match v.get("collars") {
            Some(list) => serde_json::from_value(list.clone()).context("a bulk-link response's collars")?,
            None => vec![serde_json::from_value(v).context("expected a bulk-link response, a link ({ collar, key, endpoint, public_key }) or a card payload")?],
        };
        let mut chosen: Vec<Linked> = match pick {
            Some(p) => all.into_iter().filter(|l| l.collar.id == p || l.collar.name.as_deref() == Some(p) || l.tag.as_deref() == Some(p)).collect(),
            None => all,
        };
        let l = match (chosen.len(), pick) {
            (1, _) => chosen.remove(0),
            (0, Some(p)) => bail!("no collar {p} in it"),
            (0, None) => bail!("it holds no collars"),
            (n, _) => bail!("it holds {n} collars: pick one with --collar <id, name or tag>"),
        };
        Ok(Link {
            collar_id: l.collar.id,
            herd_id: l.collar.herd_id,
            key: l.key,
            endpoint: l.endpoint.unwrap_or_default(),
            server_key: l.public_key,
            name: l.collar.name,
            tag: l.tag,
            no_endpoint: l.no_endpoint,
        })
    }

    /// The `provision` payload, in the card's key order. Firmware 0.2 takes
    /// only an `https://` endpoint, so a LAN or loopback one is refused here
    /// with the reason rather than by the collar with `bad_endpoint`.
    pub fn payload(&self) -> anyhow::Result<String> {
        if !self.endpoint.starts_with("https://") {
            if self.endpoint.is_empty() {
                let why = self.no_endpoint.as_deref().unwrap_or("The server gave no collar endpoint.");
                bail!("{why} Or pass --endpoint https://<public url>/collar/v1 (the bridge talks to --server itself, so the bench needs no https)");
            }
            bail!(
                "the collar's endpoint {} isn't https, and firmware 0.2 only stores an https one. Pass --endpoint https://<public url>/collar/v1 \
                 (the bridge talks to --server itself, so the bench needs no https)",
                self.endpoint
            );
        }
        let s = |v: &str| serde_json::to_string(v).unwrap_or_default();
        Ok(format!(
            "{{\"v\":1,\"c\":{},\"h\":{},\"k\":{},\"e\":{},\"s\":{}}}",
            s(&self.collar_id),
            s(&self.herd_id),
            s(&self.key),
            s(&self.endpoint),
            s(&self.server_key)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BULK: &str = r#"{"batch_id":"bat_1","collars":[
        {"collar":{"id":"col_a","name":"Collar 1","herd_id":"hrd_1"},"key":"ka","endpoint":"https://farm.example/collar/v1","public_key":"PK","tag":"214"},
        {"collar":{"id":"col_b","name":"Collar 2","herd_id":"hrd_1"},"key":"kb","endpoint":"https://farm.example/collar/v1","public_key":"PK"}]}"#;

    #[test]
    fn every_shape_reads() {
        assert!(Link::parse(BULK, None).unwrap_err().to_string().contains("2 collars"));
        let by_tag = Link::parse(BULK, Some("214")).unwrap();
        assert_eq!((by_tag.collar_id.as_str(), by_tag.key.as_str()), ("col_a", "ka"));
        assert_eq!(Link::parse(BULK, Some("Collar 2")).unwrap().collar_id, "col_b");
        assert!(Link::parse(BULK, Some("nope")).is_err());

        let none = r#"{"collar":{"id":"col_f","herd_id":"hrd_2"},"key":"kf","no_endpoint":"Set the collar address in Settings, Server.","public_key":"PK"}"#;
        let err = Link::parse(none, None).unwrap().payload().unwrap_err().to_string();
        assert!(err.starts_with("Set the collar address") && err.contains("--endpoint"), "{err}");

        let single = r#"{"collar":{"id":"col_c","herd_id":"hrd_2"},"key":"kc","endpoint":"http://127.0.0.1:7878/collar/v1","public_key":"PK"}"#;
        let l = Link::parse(single, None).unwrap();
        assert_eq!(l.herd_id, "hrd_2");
        assert!(l.payload().unwrap_err().to_string().contains("isn't https"));

        let claim = r#"{"unit":{"serial":"OC1"},"collar":{"id":"col_e","herd_id":"hrd_4","name":"OC1"},"v1":{"key":"ke","public_key":"PK"}}"#;
        let l = Link::parse(claim, None).unwrap();
        assert_eq!((l.collar_id.as_str(), l.key.as_str(), l.server_key.as_str()), ("col_e", "ke", "PK"));
        assert!(l.payload().unwrap_err().to_string().contains("--endpoint"));

        let card = r#"{"v":1,"c":"col_d","h":"hrd_3","k":"kd","e":"https://x/collar/v1","s":"PK"}"#;
        let l = Link::parse(card, None).unwrap();
        assert_eq!(l.payload().unwrap(), card);
    }
}
