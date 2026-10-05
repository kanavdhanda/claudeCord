//! Health probes for the hosted service: accounts and sealed secrets (pure logic), and the front door with a machine joining through a
//! browser approval while a second account sees none of it.

use super::{Feature, Probe, boxed, scratch};
use crate::control::{Control, Poll, seal};
use serde_json::json;
use std::sync::Arc;

/// Fails the probe with a message unless the condition holds.
macro_rules! ensure {
    ($c:expr, $($m:tt)*) => {
        if !$c {
            return Err(format!($($m)*));
        }
    };
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature {
            name: "accounts, device codes and sealed bot tokens",
            covers: &["control/mod", "control/seal"],
            probe: accounts_probe,
        },
        Feature {
            name: "hosted front door: a machine joins by code, accounts stay apart",
            covers: &["control/registry", "device/enroll"],
            probe: gateway_probe,
        },
    ]
}

fn accounts_probe() -> Probe {
    boxed(async {
        let c = Control::open_memory().map_err(|e| e.to_string())?;
        let a = c
            .sign_in_discord("1", "ann", 0)
            .map_err(|e| e.to_string())?;
        let b = c
            .sign_in_discord("2", "bob", 0)
            .map_err(|e| e.to_string())?;
        ensure!(a.id != b.id, "two people got the same account");
        let sid = c.create_session(&a.id, 0).map_err(|e| e.to_string())?;
        ensure!(
            c.session(&sid, 1).map_err(|e| e.to_string())?.map(|x| x.id) == Some(a.id.clone()),
            "a session did not find its account"
        );
        // A machine is approved once and its token is handed over once.
        let (device, user) = c.device_start("mac", 0).map_err(|e| e.to_string())?;
        ensure!(
            c.device_approve(&user, &a.id, "mac", 1)
                .map_err(|e| e.to_string())?,
            "approval refused"
        );
        let Poll::Approved { token, tenant, .. } =
            c.device_poll(&device, 2).map_err(|e| e.to_string())?
        else {
            return Err("an approved machine got no token".into());
        };
        ensure!(tenant == a.id, "the token belongs to the wrong account");
        ensure!(
            c.device_poll(&device, 3).map_err(|e| e.to_string())? == Poll::Gone,
            "a token was handed over twice"
        );
        ensure!(
            c.machine_for_token(&token).map_err(|e| e.to_string())?
                == Some((a.id.clone(), "mac".into())),
            "the token does not find its machine"
        );
        // Bot tokens are sealed to their owner.
        let keys = seal::LocalKeys::random();
        let sealed = seal::seal(&keys, &a.id, "bot", "SECRET").ok_or("could not seal")?;
        ensure!(
            !sealed.contains("SECRET"),
            "the sealed text shows the secret"
        );
        ensure!(
            seal::open(&keys, &a.id, "bot", &sealed).as_deref() == Some("SECRET"),
            "the owner cannot open it"
        );
        ensure!(
            seal::open(&keys, &b.id, "bot", &sealed).is_none(),
            "another account opened it"
        );
        Ok("sessions, one-time device approval, tenant-bound sealed secrets".into())
    })
}

fn gateway_probe() -> Probe {
    boxed(async {
        use crate::server::gateway::{GatewayConfig, start_gateway};
        let dir = scratch("hosted");
        let control = Arc::new(Control::open(&dir.join("control.db")).map_err(|e| e.to_string())?);
        let gw = start_gateway(
            GatewayConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                public_url: "http://127.0.0.1".into(),
                oauth: None,
                hub: crate::server::Config::default(),
                discord: Default::default(),
                dev: false,
                bucket: None,
            },
            dir,
            control.clone(),
            Arc::new(seal::LocalKeys::random()),
        )
        .await
        .map_err(|e| e.to_string())?;
        let base = format!("http://{}", gw.addr);
        let now = crate::now_ms();
        let mut sessions = Vec::new();
        for (id, name) in [("1", "ann"), ("2", "bob")] {
            let a = control
                .sign_in_discord(id, name, now)
                .map_err(|e| e.to_string())?;
            sessions.push(
                control
                    .create_session(&a.id, now)
                    .map_err(|e| e.to_string())?,
            );
        }
        let (ann, bob) = (sessions[0].clone(), sessions[1].clone());
        let http = reqwest::Client::new();
        // The browser side: when the machine shows its code, the signed-in person approves it.
        let (h2, b2) = (http.clone(), base.clone());
        let cfg = crate::device::enroll::enroll(&base, "probe-box", move |code, _| {
            let (h, b, s, code) = (h2.clone(), b2.clone(), ann.clone(), code.to_string());
            tokio::spawn(async move {
                let _ = h
                    .post(format!("{b}/api/device/approve"))
                    .header("cookie", format!("cc_session={s}"))
                    .json(&json!({"code": code, "node": "probe-box"}))
                    .send()
                    .await;
            });
        })
        .await?;
        ensure!(
            control
                .machine_for_token(&cfg.token)
                .map_err(|e| e.to_string())?
                .is_some(),
            "the enrolled token is unknown"
        );
        let machines = |s: String| {
            let (h, b) = (http.clone(), base.clone());
            async move {
                h.get(format!("{b}/api/v1/machines"))
                    .header("cookie", format!("cc_session={s}"))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())
            }
        };
        let theirs = machines(bob).await?;
        ensure!(
            theirs.as_array().is_some_and(Vec::is_empty),
            "another account saw the machine: {theirs}"
        );
        gw.shutdown().await;
        Ok("a machine joined by code; the other account saw nothing".into())
    })
}
