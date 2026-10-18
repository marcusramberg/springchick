//! The display panel's shape: corner radius and camera cutouts, from
//! gmobile's device database (what Phosh uses), matched by device-tree
//! compatible.
//!
//! `display-panels.json` is gmobile 0.7.1's `data/devices/display-panels/`
//! (LGPL-2.1-or-later) merged into one map keyed by compatible:
//! `jq -n 'reduce inputs as $p ({}; . + {(input_filename | rtrimstr(".json")): $p})' *.json`
#![forbid(unsafe_code)]

use std::collections::HashMap;

use sc_layout::Rect;
use serde::Deserialize;

const DATA: &str = include_str!("../display-panels.json");

#[derive(Deserialize)]
struct Entry {
    name: String,
    #[serde(rename = "x-res")]
    x_res: u32,
    #[serde(rename = "y-res")]
    y_res: u32,
    #[serde(rename = "border-radius")]
    border_radius: Option<f32>,
    /// Top-left first, clockwise.
    #[serde(rename = "corner-radii")]
    corner_radii: Option<[f32; 4]>,
    #[serde(default)]
    cutouts: Vec<Cutout>,
}

#[derive(Deserialize)]
struct Cutout {
    path: String,
}

/// In panel pixels, unrotated.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub name: String,
    pub x_res: u32,
    pub y_res: u32,
    /// Top-left's; 0 for square corners.
    pub corner_radius: f32,
    pub cutouts: Vec<Rect>,
}

impl Panel {
    /// Scaled onto an upright `w`×`h` output. `None` when the output isn't
    /// this panel's shape (a sideways mount, or another display).
    pub fn fit(&self, w: i32, h: i32) -> Option<Panel> {
        let (pw, ph) = (self.x_res as f32, self.y_res as f32);
        let s = w as f32 / pw;
        if s <= 0.0 || ((h as f32 / ph) - s).abs() > s * 0.01 {
            return None;
        }
        let scale = |r: &Rect| Rect {
            x: r.x * s,
            y: r.y * s,
            w: r.w * s,
            h: r.h * s,
        };
        Some(Panel {
            name: self.name.clone(),
            x_res: w as u32,
            y_res: h as u32,
            corner_radius: self.corner_radius * s,
            cutouts: self.cutouts.iter().map(scale).collect(),
        })
    }
}

/// The first compatible with an entry wins, as in gmobile.
pub fn lookup<'a>(compatibles: impl IntoIterator<Item = &'a str>) -> Option<Panel> {
    let db: HashMap<String, Entry> = serde_json::from_str(DATA).expect("bundled panel data");
    let e = compatibles.into_iter().find_map(|c| db.get(c))?;
    Some(Panel {
        name: e.name.clone(),
        x_res: e.x_res,
        y_res: e.y_res,
        corner_radius: e
            .corner_radii
            .map(|r| r[0])
            .or(e.border_radius)
            .unwrap_or(0.0),
        cutouts: e
            .cutouts
            .iter()
            .filter_map(|c| path_bounds(&c.path, e.x_res, e.y_res))
            .collect(),
    })
}

/// This machine's panel. `GMOBILE_DT_COMPATIBLES` (`:`-separated) overrides
/// the device tree, as it does for gmobile.
pub fn detect() -> Option<Panel> {
    if let Ok(env) = std::env::var("GMOBILE_DT_COMPATIBLES") {
        return lookup(env.split(':'));
    }
    let raw = std::fs::read("/sys/firmware/devicetree/base/compatible").ok()?;
    let raw = String::from_utf8_lossy(&raw);
    lookup(raw.split('\0').filter(|c| !c.is_empty()))
}

fn path_bounds(d: &str, w: u32, h: u32) -> Option<Rect> {
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><path d="{d}"/></svg>"#
    );
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let b = tree.root().abs_bounding_box();
    Some(Rect {
        x: b.x(),
        y: b.y().max(0.0),
        w: b.width(),
        h: b.bottom() - b.y().max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.5
    }

    #[test]
    fn fp5_has_a_round_camera_hole() {
        let p = lookup(["fairphone,fp5", "qcom,qcm6490"]).unwrap();
        assert_eq!((p.x_res, p.y_res), (1224, 2700));
        assert_eq!(p.corner_radius, 100.0);
        let c = p.cutouts[0];
        assert!(
            near(c.x, 570.0) && near(c.y, 16.0) && near(c.w, 84.0) && near(c.h, 84.0),
            "{c:?}"
        );
    }

    #[test]
    fn corner_radii_and_missing_radius() {
        assert_eq!(lookup(["apple,j314s"]).unwrap().corner_radius, 40.0);
        assert_eq!(lookup(["purism,librem5"]).unwrap().corner_radius, 0.0);
    }

    #[test]
    fn every_entry_parses_with_its_cutouts() {
        let db: HashMap<String, serde_json::Value> = serde_json::from_str(DATA).unwrap();
        for (k, v) in &db {
            let p = lookup([k.as_str()]).unwrap();
            let n = v["cutouts"].as_array().map_or(0, |a| a.len());
            assert_eq!(p.cutouts.len(), n, "{k}");
            for c in &p.cutouts {
                assert!(c.w > 0.0 && c.h > 0.0 && c.x >= 0.0, "{k}: {c:?}");
            }
        }
    }

    #[test]
    fn unknown_device_is_none() {
        assert_eq!(lookup(["acme,anvil"]), None);
    }

    #[test]
    fn fit_scales_and_rejects_other_shapes() {
        let p = lookup(["fairphone,fp5"]).unwrap();
        let half = p.fit(612, 1350).unwrap();
        assert_eq!(half.corner_radius, 50.0);
        assert!(near(half.cutouts[0].x, 285.0));
        assert_eq!(p.fit(2700, 1224), None);
        assert_eq!(p.fit(1920, 1080), None);
    }
}
