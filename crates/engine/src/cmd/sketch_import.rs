//! Sketch drawing exchange: Insert DXF, Insert SVG, Insert Airfoil and sketch DXF export.

use solidcraft_geom::prelude::*;
use std::collections::HashMap;

use serde_json::{Value, json};
use solidcore_record::{MAX_BEZIER_DEGREE, Vec2, Vec3};
use solidcraft_geom::bspline::{self, Fit, HPoint, NurbsCurve};
use solidcraft_io::{AirfoilFormat, DxfEntity, DxfLayer, Geom2, MAX_AIRFOIL_BYTES, MAX_DRAWING_BYTES};
use solidcraft_sketch::{Analysis, ConstraintKind, CurveKind, LinkKind, LinkSource, MAX_SPLINE_POINTS, Nurbs, Sketch};

use super::CommandSpec;
use super::sketch::{Left, edit_solved, ids_of};
use super::sketch_store;
use crate::params::{bad, bool_, num, str_, vec2};
use crate::schema::sketch_import::{
    AirfoilInserted, DrawingBoundsParams, DrawingBoundsResult, DrawingBox, DrawingInserted, DxfExported, ExportDxfParams, ExportDxfResult,
    InsertAirfoilParams, InsertDxfParams, InsertSvgParams, SketchSolve,
};
use crate::schema::{TypedContract, output_schema_for};
use crate::{EngineError, Result, Session};

const INSERT_DXF_CONTRACT: TypedContract<InsertDxfParams> = TypedContract::new();
const INSERT_SVG_CONTRACT: TypedContract<InsertSvgParams> = TypedContract::new();
const INSERT_AIRFOIL_CONTRACT: TypedContract<InsertAirfoilParams> = TypedContract::new();
const DRAWING_BOUNDS_CONTRACT: TypedContract<DrawingBoundsParams> = TypedContract::new();
const EXPORT_DXF_CONTRACT: TypedContract<ExportDxfParams> = TypedContract::new();

/// A command's reply as JSON, built from the type its output schema describes so the two can't
/// drift apart.
fn reply(cmd: &str, reply: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(reply).map_err(|e| bad(cmd, e.to_string()))
}

pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("sketch.insert_dxf", "Insert DXF", crate::checked!("sketch.insert_dxf", insert_dxf))
        .at("SKETCH", "INSERT")
        .icon("sketch_dxf")
        .contract(&INSERT_DXF_CONTRACT)
        .output(output_schema_for::<DrawingInserted>)
        .params(concat!("path | text: ASCII DXF (blocks expanded); at?: [x,y] offset, scale? | width? | height?: mm, the size of the drawn geometry's bounds before rotation, aspect kept (win over scale); angle?: degrees, about the drawing's origin (or its centre); align?: \"origin\" (default: `at` is where the file's origin goes) | \"center\" (`at` is where the drawing's middle goes and it turns and scales about it); tolerance?: mm within which ends join (default 0.0001); fit_tolerance?: mm a spline may be from the file's curve in the sketch, 0.0001 to 10 (default 0.001; half for following it, half for joining its ends; a piece that can't be followed that close comes in within a tolerance 4, 16, … times looser, and the reply's warnings give its proven distance; a spline within 1e-9 mm of a point in the sketch is left out, and the warnings count it); fixed?: bool (a Fix constraint on every point the drawing adds, as sketch.constraint.fix puts one; a circle's or ellipse's radius stays free; default false); plane?: (when no sketch is active, a new sketch on it; default XY)", created_doc!())),
    CommandSpec::new("sketch.insert_svg", "Insert SVG", crate::checked!("sketch.insert_svg", insert_svg))
        .at("SKETCH", "INSERT")
        .icon("svg")
        .contract(&INSERT_SVG_CONTRACT)
        .output(output_schema_for::<DrawingInserted>)
        .params(concat!("path | text: SVG; at?: [x,y] offset, scale? | width? | height?: mm, the drawing's size before rotation, aspect kept (win over scale); size_from?: \"geometry\" (default: width and height are the bounds of the drawn geometry) | \"viewbox\" (the SVG sheet: its viewBox, else its width and height; a drawing smaller than its sheet then comes out smaller than width or height say); angle?: degrees, about the drawing's origin (or its centre); align?: \"origin\" (default: `at` is where the file's origin goes) | \"center\" (`at` is where the drawing's (or sheet's) middle goes and it turns and scales about it); tolerance?: mm within which ends join (default 0.0001); fit_tolerance?: mm a curve may be from the file's curve in the sketch, 0.0001 to 10 (default 0.001; half for following it, half for joining its ends; a piece that can't be followed that close comes in within a tolerance 4, 16, … times looser, and the reply's warnings give its proven distance; a curve within 1e-9 mm of a point in the sketch is left out, and the warnings count it); fixed?: bool (every point it adds gets a Fix constraint, so the drawing stays where it is put); plane?: (when no sketch is active, a new sketch on it; default XY)", created_doc!())),
    CommandSpec::new("sketch.insert_airfoil", "Insert Airfoil", crate::checked!("sketch.insert_airfoil", insert_airfoil))
        .at("SKETCH", "INSERT")
        .icon("airfoil")
        .contract(&INSERT_AIRFOIL_CONTRACT)
        .output(output_schema_for::<AirfoilInserted>)
        .params(concat!("path | text: airfoil coordinates (.dat, Selig or Lednicer, told apart by content; format?: \"selig\" | \"lednicer\" overrides); chord: mm (> 0); pivot?: share of the chord that `at` places and `angle` turns about (0 leading edge, 0.25 quarter chord, 1 trailing edge; default 0); at?: [x,y] mm (default [0,0]); angle?: degrees, counter-clockwise about the pivot; flip?: bool, mirror across the chord line; tolerance?: mm within which points join (default 0.0001; a trailing edge within it is closed, else a line closes it); plane?: (when no sketch is active, a new sketch on it; default XY); name?: the new sketch's name. One fit point spline through every point; replies with the point count, the trailing edge's gap, the largest deviation of merged points and the largest bulge of the spline between points", created_doc!())),
    CommandSpec::new("sketch.drawing_bounds", "Drawing Size", crate::checked!("sketch.drawing_bounds", drawing_bounds))
        .icon("svg")
        .noundo().read_only()
        .contract(&DRAWING_BOUNDS_CONTRACT)
        .output(output_schema_for::<DrawingBoundsResult>)
        .params("path | text: SVG or DXF (by `format`: \"svg\" | \"dxf\", default by the file name, else SVG); returns min, max, width and height of the drawn geometry in the file's own size, and for an SVG with a viewBox (or width and height) its sheet: {min, max, width, height}"),
    CommandSpec::new("sketch.export_dxf", "Save As DXF", crate::checked!("sketch.export_dxf", export_dxf))
        .icon("sketch_dxf")
        .noundo()
        .contract(&EXPORT_DXF_CONTRACT)
        .output(output_schema_for::<ExportDxfResult>)
        .params("sketch?: id|name (default active); path?: file to write (else the DXF text is returned); units?: \"design\" | \"in\" | \"inch\" | \"mm\" | \"cm\" | \"m\" | \"ft\" (default mm); layer?: profile layer name (default \"0\"); layers?: \"single\" | \"roles\" (default \"single\": all on `layer`; \"roles\": outer on \"OUTLINE\", holes on \"CUT\"); join?: bool (default false: R12 LINE/ARC; true: closed loops as LWPOLYLINE with arc bulges)"),
];

/// The sketch import command for a drawing file name, regardless of extension case.
pub fn drawing_import_command(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("dxf") {
        Some("sketch.insert_dxf")
    } else if ext.eq_ignore_ascii_case("svg") {
        Some("sketch.insert_svg")
    } else {
        None
    }
}

fn source_text(p: &Value, cmd: &str, svg: bool) -> Result<String> {
    if let Some(t) = str_(p, "text") {
        if t.len() > MAX_DRAWING_BYTES {
            return Err(bad(cmd, "the drawing is too large"));
        }
        return Ok(t.to_string());
    }
    let path = str_(p, "path").filter(|x| !x.trim().is_empty() && x.len() < 4096).ok_or_else(|| bad(cmd, "give `path` or `text`"))?;
    let bytes = solidcraft_io::vfs::read(path, MAX_DRAWING_BYTES).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    if svg {
        return solidcraft_io::decode_svg(&bytes).map_err(|e| drawing_error(p, svg, e.to_string()));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// A problem with the drawing file's contents (not with the command's parameters), named after
/// the file so the message says which drawing it is about (#9641).
fn drawing_error(p: &Value, svg: bool, msg: String) -> EngineError {
    let name = str_(p, "path")
        .and_then(|x| std::path::Path::new(x).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| if svg { "SVG" } else { "DXF" }.to_string());
    EngineError::Other(format!("{name}: {msg}"))
}

/// Add imported geometry to a sketch, sharing end points that meet.
/// Adds the geometry; end points closer than `q` (mm) become one point, so drawings whose
/// ends almost meet still make closed profiles. Also returns how far that moved the end of a
/// spline or conic, which otherwise keeps the shape it was given (mm).
pub(super) fn add_geometry(sk: &mut Sketch, geom: &[Geom2], f: &dyn Fn(Vec2) -> Vec2, k: f64, q: f64) -> Result<(Vec<usize>, Vec<Geom2>, f64)> {
    // A dense drawing adds tens of thousands of points and curves: a batch finds each new id
    // without searching the whole sketch, so the insert takes time in proportion to its size.
    let sk = &mut sk.batch();
    let mut map: HashMap<(i64, i64), Vec<(Vec2, usize)>> = HashMap::new();
    let mut pt = |sk: &mut solidcraft_sketch::SketchBatch, p: Vec2| -> Result<usize> {
        let p = f(p);
        let key = ((p.x / q).floor() as i64, (p.y / q).floor() as i64);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(v) = map.get(&(key.0 + dx, key.1 + dy))
                    && let Some((_, i)) = v.iter().find(|(o, _)| o.dist(p) <= q)
                {
                    return Ok(*i);
                }
            }
        }
        let i = sk.add_point(p, None)?;
        map.entry(key).or_default().push((p, i));
        Ok(i)
    };
    // How far the point an end joined is from where the end was.
    let shift = |sk: &Sketch, p: Vec2, i: usize| sk.point(i).map_or(0.0, |o| o.dist(f(p)));
    let mut moved = 0.0f64;
    let mut curves = Vec::new();
    let mut texts = Vec::new();
    for g in geom {
        // A B-spline with knots of its own that a sketch spline holds ([`native_nurbs`]) is that one
        // spline over its control points, keeping its knots.
        let (native, knots);
        let g = match g {
            Geom2::Nurbs(curve) => {
                let (pts, degree, nurbs) = native_nurbs(curve)
                    .ok_or_else(|| EngineError::Other("a B-spline must be brought in as sketch curves first (split_nurbs)".into()))?;
                (native, knots) = (Geom2::Spline { pts, control: true, degree }, nurbs);
                &native
            }
            g => {
                knots = None;
                g
            }
        };
        let made = match g {
            Geom2::Point(p) => {
                pt(sk, *p)?;
                None
            }
            Geom2::Line(a, b) => {
                let (a, b) = (pt(sk, *a)?, pt(sk, *b)?);
                (a != b).then_some(CurveKind::Line { a, b })
            }
            Geom2::Circle(c, r) => Some(CurveKind::Circle { c: pt(sk, *c)?, r: r * k }),
            Geom2::Arc { c, a, b } => {
                let (c, a, b) = (pt(sk, *c)?, pt(sk, *a)?, pt(sk, *b)?);
                (a != b && c != a).then_some(CurveKind::Arc { c, a, b })
            }
            Geom2::Ellipse { c, major, minor } => Some(CurveKind::Ellipse { c: pt(sk, *c)?, m: pt(sk, *major)?, r: minor * k }),
            Geom2::Spline { pts, control, degree } => {
                // Only the ends join other curves: an inner point moved would reshape the curve. A
                // through-point spline keeps one of two equal points in a row; a control-point one
                // repeating a point never comes here (every caller passes its geometry through
                // [`split_nurbs`], which follows it with curves that don't), so its points are kept
                // as they are. A spline past the cap keeps every n-th point and always its last, so
                // a loop stays closed (taking the first ones only cut its tail off, #11119).
                let cap = MAX_SPLINE_POINTS;
                let mut pts: Vec<Vec2> = if pts.len() > cap {
                    (0..cap).filter_map(|k| pts.get((k * (pts.len() - 1) + (cap - 1) / 2) / (cap - 1))).copied().collect()
                } else {
                    pts.clone()
                };
                if !*control {
                    pts.dedup();
                }
                match (pts.first(), pts.last()) {
                    (Some(&first), Some(&last)) if pts.iter().any(|q| *q != first) => {
                        let (a, b) = (pt(sk, first)?, pt(sk, last)?);
                        moved = moved.max(shift(sk.sketch(), first, a)).max(shift(sk.sketch(), last, b));
                        let inner = pts.get(1..pts.len() - 1).unwrap_or_default();
                        if inner.is_empty() && a == b {
                            None
                        } else {
                            let mut idx = vec![a];
                            for q in inner {
                                idx.push(sk.add_point(f(*q), None)?);
                            }
                            idx.push(b);
                            Some(CurveKind::Spline {
                                pts: idx,
                                control: *control,
                                degree: *degree,
                                periodic: false,
                                nurbs: knots,
                                handles: Vec::new(),
                            })
                        }
                    }
                    _ => None,
                }
            }
            // Replaced by its spline above.
            Geom2::Nurbs(_) => None,
            Geom2::Conic { a, apex, b, rho } => {
                // The apex shapes the curve: it joins nothing.
                let (a0, b0) = (*a, *b);
                let (a, b) = (pt(sk, a0)?, pt(sk, b0)?);
                moved = moved.max(shift(sk.sketch(), a0, a)).max(shift(sk.sketch(), b0, b));
                if a == b { None } else { Some(CurveKind::Conic { a, b, apex: sk.add_point(f(*apex), None)?, rho: *rho }) }
            }
            Geom2::Text { text, at, height, angle } => {
                texts.push(Geom2::Text { text: text.clone(), at: f(*at), height: height * k, angle: *angle });
                None
            }
        };
        if let Some(kind) = made
            && let Ok(c) = sk.add_curve(kind, None)
        {
            curves.push(c);
        }
    }
    Ok((curves, texts, moved))
}

/// The box around a drawing's geometry (arcs, circles and curves by their extent).
pub(super) fn bounds_of(geom: &[Geom2]) -> Option<(Vec2, Vec2)> {
    let mut pts: Vec<Vec2> = Vec::new();
    for g in geom {
        match g {
            Geom2::Point(p) => pts.push(*p),
            Geom2::Line(a, b) => pts.extend([*a, *b]),
            Geom2::Circle(c, r) => pts.extend([*c + Vec2::new(*r, *r), *c - Vec2::new(*r, *r)]),
            Geom2::Arc { c, a, b } => {
                let r = c.dist(*a);
                let (a0, mut a1) = ((*a - *c).angle(), (*b - *c).angle());
                while a1 <= a0 {
                    a1 += std::f64::consts::TAU;
                }
                pts.extend([*a, *b]);
                // The four extreme directions that lie on the arc.
                for q in 0..4 {
                    let mut d = f64::from(q) * std::f64::consts::FRAC_PI_2;
                    while d < a0 {
                        d += std::f64::consts::TAU;
                    }
                    if d < a1 {
                        pts.push(*c + Vec2::from_angle(d) * r);
                    }
                }
            }
            Geom2::Ellipse { c, major, minor } => {
                let r = c.dist(*major).max(*minor);
                pts.extend([*c + Vec2::new(r, r), *c - Vec2::new(r, r)]);
            }
            Geom2::Spline { pts: sp, .. } => pts.extend(sp.iter().copied()),
            // A B-spline lies within its control points' hull.
            Geom2::Nurbs(curve) => pts.extend(curve.ctrl().filter_map(|h| h.project()).map(|q| Vec2::new(q.x, q.y))),
            // Points along the curve, not its apex, which can lie well outside it (an elliptical
            // arc of a DXF comes in as conics, #8155).
            Geom2::Conic { a, apex, b, rho } if *rho > 0.0 && *rho < 1.0 => {
                let seg = solidcore_record::Seg2::Conic { a: *a, apex: *apex, b: *b, w: rho / (1.0 - rho) };
                pts.extend((0..=64).map(|i| seg.point_at(f64::from(i) / 64.0)));
            }
            Geom2::Conic { a, apex, b, .. } => pts.extend([*a, *apex, *b]),
            Geom2::Text { at, .. } => pts.push(*at),
        }
    }
    let first = *pts.first()?;
    let (lo, hi) = pts.iter().fold((first, first), |(lo, hi), p| (Vec2::new(lo.x.min(p.x), lo.y.min(p.y)), Vec2::new(hi.x.max(p.x), hi.y.max(p.y))));
    (lo.x.is_finite() && lo.y.is_finite() && hi.x.is_finite() && hi.y.is_finite()).then_some((lo, hi))
}

/// A drawing as read: its geometry, the box around it, its text, and what the reading warns
/// about (an SVG cubic kept as drawn that the curve check refuses, #16190).
type Drawing = (Vec<Geom2>, (Vec2, Vec2), String, Vec<String>);

/// The drawing's geometry and the box around it; a drawing with none is an error that says why
/// (#9641).
fn read_drawing(p: &Value, cmd: &str, svg: bool) -> Result<Drawing> {
    let text = source_text(p, cmd, svg)?;
    let (geom, warnings) = if svg {
        solidcraft_io::read_svg_drawing(&text).map(|d| (d.geometry, d.warnings.iter().map(ToString::to_string).collect()))
    } else {
        solidcraft_io::read_dxf(&text).map(|g| (g, Vec::new()))
    }
    .map_err(|e| drawing_error(p, svg, e.to_string()))?;
    let Some(bounds) = bounds_of(&geom) else {
        let why = if svg { solidcraft_io::svg_nothing_to_import(&text) } else { "the DXF has no entities SolidCraft reads".to_string() };
        return Err(drawing_error(p, svg, why));
    };
    Ok((geom, bounds, text, warnings))
}

/// The box around a drawing, as the reply describes it: `min`, `max` and the span between them.
fn drawing_box((lo, hi): (Vec2, Vec2)) -> DrawingBox {
    DrawingBox { min: [lo.x, lo.y], max: [hi.x, hi.y], width: hi.x - lo.x, height: hi.y - lo.y }
}

/// How far (mm, in the sketch) a B-spline may come in from the file's curve unless `fit_tolerance`
/// says otherwise: half of it for the curves that follow it, half for joining their ends to other
/// curves. An end moved further to join another (a join tolerance above that) is reported.
pub(super) const DEFAULT_FIT_TOLERANCE: f64 = 1e-3;
/// Most sketch curves one Bézier piece of a B-spline may become.
const MAX_PIECE_CURVES: usize = 4096;
/// Most sketch curves a drawing's B-splines may become together: a bound on the work and memory
/// a hostile file can ask for.
const MAX_SPLINE_CURVES: usize = 200_000;
/// How much a tolerance a piece can't be followed within is loosened each time, and how many
/// times at most: from 0.0005 mm, past a kilometre.
const LOOSEN: f64 = 4.0;
const MAX_LOOSENINGS: usize = 32;

/// The `fit_tolerance` asked for (mm), within the bounds an equation curve's tolerance has.
fn fit_tolerance(p: &Value, cmd: &str) -> Result<f64> {
    use solidcraft_doc::sketch_equation::{MAX_TOLERANCE, MIN_TOLERANCE};
    match p.get("fit_tolerance") {
        None | Some(Value::Null) => Ok(DEFAULT_FIT_TOLERANCE),
        Some(v) => v
            .as_f64()
            .filter(|t| t.is_finite() && (MIN_TOLERANCE..=MAX_TOLERANCE).contains(t))
            .ok_or_else(|| bad(cmd, format!("`fit_tolerance` must be a number of mm from {MIN_TOLERANCE} to {MAX_TOLERANCE}"))),
    }
}

/// Whether two neighbouring points of a control-point spline are nearer than `gap` (in the
/// drawing: [`bspline::MIN_CONTROL_GAP`] in the sketch), which the sketch's own splines don't
/// allow: at an end it leaves the curve no direction there.
fn repeats(pts: &[Vec2], gap: f64) -> bool {
    pts.windows(2).any(|w| w[0].dist(w[1]) < gap)
}

/// Whether every one of `pts` lies nearer than `gap` to the first: a curve that small is a point.
fn is_point(pts: &[Vec2], gap: f64) -> bool {
    pts.first().is_none_or(|&o| pts.iter().all(|p| p.dist(o) < gap))
}

/// Whether every control point of `fit` lies nearer than `gap` to its first ([`is_point`]).
fn fit_is_point(fit: &Fit, gap: f64) -> bool {
    let conic;
    let pts: &[Vec3] = match fit {
        Fit::Bezier(p) => p,
        Fit::Conic { a, apex, b, .. } => {
            conic = [*a, *apex, *b];
            &conic
        }
    };
    pts.first().is_none_or(|&o| pts.iter().all(|p| p.dist(o) < gap))
}

/// [`bspline::follow`] within `tolerance`, no two neighbouring points nearer than `gap`, or, when
/// the piece can't be followed that close in `room` curves, within the first of the tolerances
/// [`LOOSEN`] times looser, up to [`MAX_LOOSENINGS`] times, that it can be: not the closest fit
/// there is, but one with its bound proven. With whether it was loosened. A piece still not
/// followed then, or that can't be at all (its points don't project, its weights lose their
/// precision), is refused with the last reason.
fn follow_loosened(
    piece: &bspline::CurvePiece,
    tolerance: f64,
    gap: f64,
    room: usize,
) -> bspline::SplineResult<(Vec<bspline::Certified<Fit>>, bool)> {
    let (mut at, mut loosenings) = (tolerance, 0);
    loop {
        match bspline::follow(piece, at, gap, MAX_BEZIER_DEGREE, room) {
            Ok(fits) => return Ok((fits, at > tolerance)),
            Err(bspline::SplineError::TooLarge(_) | bspline::SplineError::Unsupported(_)) if loosenings < MAX_LOOSENINGS => {
                loosenings += 1;
                at *= LOOSEN;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A drawing's B-splines as the curves a sketch holds. One that is the sketch's own spline
/// ([`sketch_own`]) comes in as that one editable spline. Any other is split into Bézier pieces,
/// each followed within `tolerance` (mm, in the drawing) of the file's curve with the bound proven
/// ([`bspline::follow`]): a polynomial piece of degree at most 9 as a control-point spline (a line
/// at degree 1), a rational quadratic one as a conic, any other as cubic control-point splines.
/// That is every [`Geom2::Nurbs`], every control-point spline with more points than a sketch
/// spline holds, which would otherwise lose the rest, and every one that repeats a control point,
/// which a sketch spline doesn't (a zero-length handle leaves its end no direction, and the solid
/// kernel refuses to build on it). Points repeat when they are nearer than
/// [`bspline::MIN_CONTROL_GAP`] in the sketch, which is that over `k` in the drawing. A spline
/// whose control points all lie that near one another is a point, not a curve: it is left out, as
/// is a fit of a piece that small (the curves either side of it meet across it). A piece that
/// can't be followed within the tolerance comes in within a looser one, its bound proven
/// ([`follow_loosened`]). Also what the import should say about them, its distances in the
/// sketch's millimetres (`k` per drawing millimetre), and how many curves they became. A spline
/// that isn't a curve, can't be followed even loosened, or makes more curves than a drawing may,
/// refuses the drawing with the reason.
pub(super) fn split_nurbs(geom: Vec<Geom2>, tolerance: f64, k: f64) -> std::result::Result<(Vec<Geom2>, Vec<String>, usize), String> {
    let bad = |e: &dyn std::fmt::Display| format!("a spline can't be brought in: {e}");
    let gap = bspline::MIN_CONTROL_GAP / k;
    let mut out = Vec::with_capacity(geom.len());
    let (mut fitted, mut worst, mut made, mut repeated) = (0usize, 0.0f64, 0usize, 0usize);
    let (mut loosened, mut loosest, mut dropped) = (0usize, 0.0f64, 0usize);
    for g in geom {
        let curve = match g {
            Geom2::Nurbs(curve) => curve,
            Geom2::Spline { pts, control: true, .. } if is_point(&pts, gap) => {
                dropped += 1;
                continue;
            }
            Geom2::Spline { pts, control: true, degree } if pts.len() > MAX_SPLINE_POINTS || repeats(&pts, gap) => {
                sketch_spline(&pts, degree).map_err(|e| bad(&e))?
            }
            g => {
                out.push(g);
                continue;
            }
        };
        let ctrl = flat_ctrl(&curve);
        if is_point(&ctrl, gap) {
            dropped += 1;
            continue;
        }
        if let Some((pts, degree, knots)) = native_nurbs(&curve).filter(|(pts, ..)| !repeats(pts, gap)) {
            made += 1;
            out.push(if knots.is_some() { Geom2::Nurbs(curve) } else { Geom2::Spline { pts, control: true, degree } });
            continue;
        }
        repeated += usize::from(repeats(&ctrl, gap));
        for piece in curve.pieces().map_err(|e| bad(&e))? {
            if made >= MAX_SPLINE_CURVES {
                return Err(format!("the drawing's splines make more than {MAX_SPLINE_CURVES} curves"));
            }
            let (fits, wider) = follow_loosened(&piece, tolerance, gap, MAX_PIECE_CURVES.min(MAX_SPLINE_CURVES - made)).map_err(|e| bad(&e))?;
            let fits: Vec<bspline::Certified<Fit>> = fits.into_iter().filter(|c| !fit_is_point(c.value(), gap)).collect();
            if wider {
                loosened += 1;
                loosest = fits.iter().fold(loosest, |w, c| w.max(c.bound()));
            }
            // The piece itself: over its own control points, or a conic; or nothing, a point.
            let own_points: Vec<Vec3> = piece.ctrl().filter_map(|h| h.project()).collect();
            let own = match &fits[..] {
                [] => true,
                [c] => matches!(c.value(), Fit::Conic { .. }) || *c.value() == Fit::Bezier(own_points),
                _ => false,
            };
            if !own {
                fitted += 1;
                worst = fits.iter().fold(worst, |w, c| w.max(c.bound()));
            }
            made += fits.len();
            out.extend(fits.into_iter().map(|c| sketch_curve(c.into_value())));
        }
    }
    // A fit's bound is how closely it follows the file's curve; joining its ends to other curves
    // can move them further, up to the join tolerance (reported by itself past the fit tolerance).
    let join = "an end joined to another curve can move further, up to the join tolerance";
    let mut warnings = Vec::new();
    if dropped > 0 {
        warnings.push(format!(
            "{dropped} spline(s) left out: each lies within {:.0e} mm in the sketch, the least gap between a sketch spline's points, so it is a point, not a curve",
            bspline::MIN_CONTROL_GAP
        ));
    }
    if repeated > 0 {
        warnings.push(format!(
            "{repeated} spline(s) repeat a control point, which a sketch spline can't (a zero-length handle leaves its end no direction): each comes in as curves that follow it"
        ));
    }
    if fitted > 0 {
        warnings.push(format!(
            "{fitted} spline piece(s) have no exact sketch curve: each comes in as cubic splines followed within {:.1e} mm of the file's curve ({join})",
            worst * k
        ));
    }
    if loosened > 0 {
        warnings.push(format!(
            "{loosened} spline piece(s) couldn't be followed within the fit tolerance of {:.1e} mm: they come in followed within {:.1e} mm of the file's curve ({join})",
            2.0 * tolerance * k,
            loosest * k
        ));
    }
    Ok((out, warnings, made))
}

/// The knots of the sketch's spline of degree `p` over `n > p` points on `[lo, hi]`: each end
/// `p + 1` times, the inner ones evenly spaced.
fn sketch_knots(p: usize, n: usize, lo: f64, hi: f64) -> Vec<f64> {
    let spans = n.saturating_sub(p).max(1);
    (0..n + p + 1).map(|i| lo + (hi - lo) * (i.saturating_sub(p).min(spans) as f64 / spans as f64)).collect()
}

/// The sketch's own spline of `degree` over `pts` (clamped, its inner knots evenly spaced, at the
/// degree its points allow), as a B-spline.
fn sketch_spline(pts: &[Vec2], degree: u8) -> bspline::SplineResult<NurbsCurve> {
    let n = pts.len();
    let p = usize::from(degree).clamp(1, MAX_BEZIER_DEGREE).min(n.saturating_sub(1)).max(1);
    let ctrl: Vec<HPoint> = pts.iter().map(|q| HPoint::weighted(Vec3::new(q.x, q.y, 0.0), 1.0)).collect();
    NurbsCurve::from_homogeneous(p, sketch_knots(p, n, 0.0, 1.0), &ctrl)
}

/// `curve` as one sketch spline over its control points, when a sketch holds it as it is: degree
/// at most 9, at most 500 points, clamped knots no inner one of which repeats more than the
/// degree ([`Nurbs::check`]), its weights all alike (which is no weighting). The points, the
/// degree, and the knots when they aren't the sketch's own, evenly spaced up to the rounding of a
/// number (4ε of the domain), which a spline without any has: knots written to fewer digits than
/// that are a different curve. A rational curve isn't: it has no exact pieces but at degree 2,
/// where it is as many conics as it has spans, which the sketch holds without a spline's knots.
/// None for a curve with other knots, or whose control points repeat (the caller checks [`repeats`]
/// at the gap it takes).
fn native_nurbs(curve: &NurbsCurve) -> Option<(Vec<Vec2>, u8, Option<Nurbs>)> {
    let p = curve.degree();
    let degree = u8::try_from(p).ok().filter(|_| p <= MAX_BEZIER_DEGREE)?;
    let ctrl: Vec<HPoint> = curve.ctrl().collect();
    let n = ctrl.len();
    if n > MAX_SPLINE_POINTS || ctrl.windows(2).any(|w| w[0].w != w[1].w) {
        return None;
    }
    let k = curve.knots();
    let (lo, hi) = (*k.get(p)?, *k.get(n)?);
    let pts = ctrl.iter().map(|h| h.project().map(|q| Vec2::new(q.x, q.y))).collect::<Option<Vec<_>>>()?;
    if sketch_knots(p, n, lo, hi).iter().zip(k).all(|(a, b)| (a - b).abs() <= 4.0 * f64::EPSILON * (hi - lo)) {
        return Some((pts, degree, None));
    }
    let nurbs = Nurbs { knots: k.to_vec(), weights: Vec::new() };
    nurbs.check(n, degree).ok()?;
    Some((pts, degree, Some(nurbs)))
}

/// The control points of `curve` in the drawing's plane.
fn flat_ctrl(curve: &NurbsCurve) -> Vec<Vec2> {
    curve.ctrl().filter_map(|h| h.project()).map(|q| Vec2::new(q.x, q.y)).collect()
}

/// A curve a sketch holds for a fit (z dropped: the file's curves have z = 0).
fn sketch_curve(fit: Fit) -> Geom2 {
    let flat = |q: Vec3| Vec2::new(q.x, q.y);
    match fit {
        Fit::Bezier(p) => match &p[..] {
            [a, b] => Geom2::Line(flat(*a), flat(*b)),
            _ => Geom2::Spline {
                degree: u8::try_from(p.len().saturating_sub(1)).unwrap_or(u8::MAX),
                control: true,
                pts: p.into_iter().map(flat).collect(),
            },
        },
        Fit::Conic { a, apex, b, rho } => Geom2::Conic { a: flat(a), apex: flat(apex), b: flat(b), rho },
    }
}

/// How big a drawing is in its own file, to size it before it is inserted.
fn drawing_bounds(_s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "sketch.drawing_bounds";
    let name_is_dxf = str_(p, "path").is_some_and(|x| x.to_ascii_lowercase().ends_with(".dxf"));
    let svg = match str_(p, "format") {
        Some(f) => !f.eq_ignore_ascii_case("dxf"),
        None => !name_is_dxf,
    };
    let (_, bounds, text, _) = read_drawing(p, cmd, svg)?;
    // Only an SVG names a sheet, and only when it says how big its sheet is.
    let sheet = svg.then(|| solidcraft_io::svg_sheet(&text)).flatten().map(drawing_box);
    reply(cmd, DrawingBoundsResult { drawing: drawing_box(bounds), sheet, param_warnings: Vec::new() })
}

fn insert(s: &mut Session, p: &Value, cmd: &str, svg: bool) -> Result<Value> {
    let (geom, (lo, hi), text, mut warnings) = read_drawing(p, cmd, svg)?;
    // What `width`, `height` and `align: center` refer to: the drawn geometry, or the SVG sheet.
    let (lo, hi) = match str_(p, "size_from") {
        None | Some("geometry") => (lo, hi),
        Some("viewbox") if !svg => return Err(bad(cmd, "`size_from: viewbox` is for SVG drawings; a DXF has no sheet")),
        Some("viewbox") => solidcraft_io::svg_sheet(&text).ok_or_else(|| bad(cmd, "the SVG has no viewBox or width and height to size by"))?,
        Some(_) => return Err(bad(cmd, "`size_from` must be \"geometry\" or \"viewbox\"")),
    };
    // A size in millimetres wins over a factor; the aspect ratio is always kept.
    let natural = (hi.x - lo.x, hi.y - lo.y);
    let sized = |key: &str, of: fn(&(f64, f64)) -> f64| -> Result<Option<f64>> {
        let Some(v) = p.get(key) else { return Ok(None) };
        let want = v.as_f64().filter(|w| w.is_finite() && *w > 0.0).ok_or_else(|| bad(cmd, format!("`{key}` must be a positive number (mm)")))?;
        let have = Some(of(&natural)).filter(|h| *h > 1e-9).ok_or_else(|| bad(cmd, format!("the drawing has no {key} to size")))?;
        Ok(Some(want / have))
    };
    let from_size = match sized("width", |n| n.0)? {
        Some(k) => Some(k),
        None => sized("height", |n| n.1)?,
    };
    let k = from_size.or_else(|| num(p, "scale")).filter(|k| *k > 1e-9 && *k < 1e6).unwrap_or(1.0);
    // The sketch's curves are the drawing's scaled by `k`: half of the fit tolerance there.
    let fit = fit_tolerance(p, cmd)?;
    let (geom, spline_warnings, from_bsplines) = split_nurbs(geom, fit / 2.0 / k, k).map_err(|e| drawing_error(p, svg, e))?;
    warnings.extend(spline_warnings);
    let angle = match p.get("angle") {
        Some(v) => v.as_f64().filter(|a| a.is_finite()).ok_or_else(|| bad(cmd, "`angle` must be a number (degrees)"))?.to_radians(),
        None => 0.0,
    };
    let pivot = match str_(p, "align") {
        None | Some("origin") => Vec2::ZERO,
        Some("center") => (lo + hi) * 0.5,
        Some(_) => return Err(bad(cmd, "`align` must be \"origin\" or \"center\"")),
    };
    let off = p.get("at").and_then(vec2).unwrap_or(Vec2::ZERO);
    let mut created = None;
    if s.active_sketch.is_none() {
        let mut cp = json!({"plane": p.get("plane").cloned().unwrap_or(json!("XY")), "project_edges": false});
        if let Some(n) = str_(p, "name") {
            cp["name"] = json!(n);
        }
        let r = (super::find_command("sketch.create").ok_or_else(|| bad(cmd, "sketch.create"))?.run)(s, &cp)?;
        created = r.get("sketch").and_then(Value::as_u64);
    }
    let tol = num(p, "tolerance").filter(|t| t.is_finite() && *t > 0.0).unwrap_or(1e-4).clamp(1e-9, 10.0);
    let f = placement(pivot, Vec2::new(k, k), angle, off);
    // A dense SVG/DXF (a floorplan) can yield far more entities than a sketch takes; refuse
    // before adding so the message names the cap (#10874). Scale alone does not shrink the count.
    if geom.len() > solidcraft_sketch::limits::max_points() {
        return Err(solidcraft_sketch::SketchError::TooLarge.into());
    }
    let fixed = bool_(p, "fixed").unwrap_or(false);
    // `sketch::edit`, but keeping the solve as a typed value for the reply.
    let target = sketch_store::target(s, p, cmd)?;
    let ((curves, texts, n_fixed, moved), report) = edit_solved(s, target, false, Analysis::Freedom, |sk, _| {
        let first = sk.points.len();
        let (cs, texts, moved) = add_geometry(sk, &geom, &f, k, tol)?;
        // Fixed where it is put: a Fix constraint on every point the drawing added (a circle's
        // or ellipse's radius stays free). Refused as a whole when the constraints would not fit.
        let mut n_fixed = 0;
        if fixed {
            let limit = solidcraft_sketch::limits::max_constraints();
            if sk.constraints.len() + (sk.points.len() - first) > limit {
                return Err(bad(cmd, format!("fixing the drawing's points needs more than the sketch's {limit} constraints")));
            }
            for q in first..sk.points.len() {
                sk.add_constraint(ConstraintKind::Fix { p: q }, None)?;
                n_fixed += 1;
            }
        }
        Ok(((ids_of(sk, &cs), texts, n_fixed, moved), Left::Unsolved))
    })?;
    let info = report.map(|rep| SketchSolve { solved: rep.keeps_orientation(), dof: rep.dof });
    if from_bsplines > 0 && moved > fit / 2.0 {
        warnings.push(format!(
            "curve ends moved up to {moved:.1e} mm to join others (the join tolerance is {tol} mm), so a spline from the file can be that far from it at an end"
        ));
    }
    // Text becomes sketch text.
    let id = sketch_store::active(s).ok_or_else(|| bad(cmd, "no sketch"))?;
    let mut n_text = 0;
    for t in texts {
        if let Geom2::Text { text, at, height, angle: own } = t {
            // Not `sketch_store::update`: a text that can't be linked is skipped, not refused.
            let mut doc = (*s.doc).clone();
            let mut sk = sketch_store::read(&doc, id)?.clone();
            if super::sketch_project::add_link(
                s,
                &doc,
                id,
                &mut sk,
                LinkKind::Text,
                LinkSource::Text { text, at, height, angle: own + angle, font: None, style: Default::default(), exprs: Default::default() },
            )
            .is_ok()
            {
                sketch_store::write(&mut doc, id, sk)?;
                *s.doc_mut() = doc;
                n_text += 1;
            }
        }
    }
    reply(
        cmd,
        DrawingInserted {
            sketch: id,
            created_sketch: created,
            curves: curves.len(),
            texts: n_text,
            fixed: fixed.then_some(n_fixed),
            info,
            created: None,
            warnings,
            param_warnings: Vec::new(),
        },
    )
}

fn insert_dxf(s: &mut Session, p: &Value) -> Result<Value> {
    insert(s, p, "sketch.insert_dxf", false)
}

fn insert_svg(s: &mut Session, p: &Value) -> Result<Value> {
    insert(s, p, "sketch.insert_svg", true)
}

/// The airfoil file's text, from `text` or the file at `path`.
fn airfoil_text(p: &Value, cmd: &str) -> Result<String> {
    if let Some(t) = str_(p, "text") {
        if t.len() > MAX_AIRFOIL_BYTES {
            return Err(bad(cmd, "the airfoil file is too large"));
        }
        return Ok(t.to_string());
    }
    let path = str_(p, "path").filter(|x| !x.trim().is_empty() && x.len() < 4096).ok_or_else(|| bad(cmd, "give `path` or `text`"))?;
    let bytes = solidcraft_io::vfs::read(path, MAX_AIRFOIL_BYTES).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// A problem with the airfoil file's contents, named after the file (or "airfoil" for text).
fn airfoil_error(p: &Value, msg: String) -> EngineError {
    let name = str_(p, "path")
        .and_then(|x| std::path::Path::new(x).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "airfoil".to_string());
    EngineError::Other(format!("{name}: {msg}"))
}

/// Where a point of an inserted file lands in the sketch: moved so `pivot` is at the origin,
/// scaled per axis by `scale` (a negative `scale.y` mirrors), turned by `angle` (radians,
/// counter-clockwise) and placed at `offset`. The one placement of every insert here.
fn placement(pivot: Vec2, scale: Vec2, angle: f64, offset: Vec2) -> impl Fn(Vec2) -> Vec2 {
    let (sin, cos) = angle.sin_cos();
    move |q: Vec2| {
        let v = Vec2::new((q.x - pivot.x) * scale.x, (q.y - pivot.y) * scale.y);
        Vec2::new(v.x * cos - v.y * sin, v.x * sin + v.y * cos) + offset
    }
}

/// Insert an airfoil coordinate file as one fit point spline through its points, at the chord
/// given, with a line across an open trailing edge (#14202).
fn insert_airfoil(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "sketch.insert_airfoil";
    let text = airfoil_text(p, cmd)?;
    let chord = p
        .get("chord")
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && *c > 0.0)
        .ok_or_else(|| bad(cmd, "`chord` must be a positive number (mm)"))?;
    let pivot = match p.get("pivot") {
        Some(v) => v.as_f64().filter(|a| a.is_finite()).ok_or_else(|| bad(cmd, "`pivot` must be a number (a share of the chord)"))?,
        None => 0.0,
    };
    let angle = match p.get("angle") {
        Some(v) => v.as_f64().filter(|a| a.is_finite()).ok_or_else(|| bad(cmd, "`angle` must be a number (degrees)"))?.to_radians(),
        None => 0.0,
    };
    let flip = match p.get("flip").filter(|v| !v.is_null()) {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(cmd, "`flip` must be true or false")),
    };
    let format = match p.get("format").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().and_then(AirfoilFormat::parse).ok_or_else(|| bad(cmd, "`format` must be \"selig\" or \"lednicer\""))?),
    };
    let tol = num(p, "tolerance").filter(|t| t.is_finite() && *t > 0.0).unwrap_or(1e-4).clamp(1e-9, 10.0);
    let off = p.get("at").and_then(vec2).unwrap_or(Vec2::ZERO);
    let airfoil = solidcraft_io::read_airfoil(&text, format).map_err(|e| airfoil_error(p, e.to_string()))?;
    if airfoil.points.len() > solidcraft_sketch::MAX_SPLINE_POINTS {
        return Err(airfoil_error(p, format!("{} points; a spline takes at most {}", airfoil.points.len(), solidcraft_sketch::MAX_SPLINE_POINTS)));
    }
    // Chord units to the sketch: scaled to the chord, mirrored when asked, turned about the pivot
    // and placed.
    let place = placement(Vec2::new(pivot, 0.0), Vec2::new(chord, if flip { -chord } else { chord }), angle, off);
    let file_points: Vec<Vec2> = airfoil.points.iter().map(|q| place(*q)).collect();
    // Points within the tolerance of the one before become one sketch point, as `add_geometry`
    // would make them; doing it here says how many, and leaves the spline's points known.
    let mut spline: Vec<Vec2> = Vec::with_capacity(file_points.len());
    // Each merged point, with the index of the spline point that stands for it.
    let mut near: Vec<(usize, [f64; 3])> = Vec::new();
    for q in &file_points {
        if spline.last().is_some_and(|k| k.dist(*q) <= tol) {
            near.push((spline.len() - 1, [q.x, q.y, 0.0]));
        } else {
            spline.push(*q);
        }
    }
    let merged = near.len();
    let (Some(first), Some(last)) = (spline.first().copied(), spline.last().copied()) else {
        return Err(airfoil_error(p, "no points".into()));
    };
    let te_gap = first.dist(last);
    let closed = te_gap <= tol;
    if closed {
        // The spline ends where it starts: a sharp trailing edge, not a periodic (rounded) one.
        spline.pop();
        spline.push(first);
        if last != first {
            near.push((spline.len() - 1, [last.x, last.y, 0.0]));
        }
    }
    if spline.len() < 4 {
        return Err(airfoil_error(p, format!("{} points left within a tolerance of {tol} mm; an airfoil needs at least four", spline.len())));
    }
    let mut geom = vec![Geom2::Spline { pts: spline.clone(), control: false, degree: 3 }];
    if !closed {
        geom.push(Geom2::Line(last, first));
    }
    // The merged points against the spline points that stand for them, and the spline against
    // the straight lines between its points: the kernel bounds both.
    let pieces: Vec<[[f64; 3]; 4]> = solidcraft_sketch::fit_beziers(&spline).iter().map(|piece| piece.map(|c| [c.x, c.y, 0.0])).collect();
    let fit = solidcore_ops::curve_fit::fit_spline_bounds(&pieces, &near).map_err(|e| airfoil_error(p, e.to_string()))?;

    let mut created = None;
    if s.active_sketch.is_none() {
        let mut cp = json!({"plane": p.get("plane").cloned().unwrap_or(json!("XY")), "project_edges": false});
        if let Some(n) = str_(p, "name") {
            cp["name"] = json!(n);
        }
        let r = (super::find_command("sketch.create").ok_or_else(|| bad(cmd, "sketch.create"))?.run)(s, &cp)?;
        created = r.get("sketch").and_then(Value::as_u64);
    }
    let target = sketch_store::target(s, p, cmd)?;
    let (curves, report) = edit_solved(s, target, false, Analysis::Freedom, |sk, _| {
        let (cs, _, _) = add_geometry(sk, &geom, &|q| q, 1.0, tol)?;
        Ok((ids_of(sk, &cs), Left::Unsolved))
    })?;
    let info = report.map(|rep| SketchSolve { solved: rep.keeps_orientation(), dof: rep.dof });
    reply(
        cmd,
        AirfoilInserted {
            sketch: target,
            created_sketch: created,
            curves: curves.len(),
            name: airfoil.name,
            format: airfoil.format.name().to_string(),
            points: airfoil.points.len(),
            merged,
            trailing_edge: if closed { "closed" } else { "open" }.to_string(),
            te_gap,
            deviation: fit.deviation,
            bulge: fit.bulge,
            info,
            created: None,
            param_warnings: Vec::new(),
        },
    )
}

fn sorted_curve_ids<'a>(ids: &'a [String]) -> Vec<&'a str> {
    let mut v: Vec<&'a str> = ids.iter().map(String::as_str).collect();
    v.sort_unstable();
    v
}

fn export_dxf(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "sketch.export_dxf";
    let id = sketch_store::target(s, p, cmd)?;
    let st = s.model.state();
    let ss = st.sketch(id).ok_or_else(|| EngineError::Other("that sketch is not evaluated".into()))?;
    let sk = &ss.sketch;

    let units_val = p.get("units").filter(|v| !v.is_null());
    if let Some(v) = units_val
        && !v.is_string()
    {
        return Err(bad(cmd, "`units` must be a string"));
    }
    let units_param = str_(p, "units").map(str::to_ascii_lowercase);
    let (insunits, scale) = match units_param.as_deref() {
        None => (4, 1.0),
        Some("design") => {
            let doc_units = solidcraft_doc::expr::length_unit_or_mm(&s.doc.units);
            match doc_units.to_ascii_lowercase().as_str() {
                "in" | "inch" => (1, 1.0 / 25.4),
                "ft" => (2, 1.0 / 304.8),
                "cm" => (5, 0.1),
                "m" => (6, 0.001),
                _ => (4, 1.0),
            }
        }
        Some("in" | "inch") => (1, 1.0 / 25.4),
        Some("ft") => (2, 1.0 / 304.8),
        Some("mm") => (4, 1.0),
        Some("cm") => (5, 0.1),
        Some("m") => (6, 0.001),
        Some(other) => {
            return Err(bad(cmd, format!("unsupported unit `{other}` (use \"design\", \"mm\", \"in\", \"cm\", \"m\", or \"ft\")")));
        }
    };

    let layer_val = p.get("layer").filter(|v| !v.is_null());
    if let Some(v) = layer_val
        && !v.is_string()
    {
        return Err(bad(cmd, "`layer` must be a string"));
    }
    let custom_layer = str_(p, "layer").map(str::trim).filter(|s| !s.is_empty());

    let layers_val = p.get("layers").filter(|v| !v.is_null());
    if let Some(v) = layers_val
        && !v.is_string()
    {
        return Err(bad(cmd, "`layers` must be \"single\" or \"roles\""));
    }
    let roles_layers = match str_(p, "layers").map(str::to_ascii_lowercase).as_deref() {
        None | Some("single") => false,
        Some("roles") => true,
        Some(_) => return Err(bad(cmd, "`layers` must be \"single\" or \"roles\"")),
    };

    let join = match p.get("join").filter(|v| !v.is_null()) {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(cmd, "`join` must be true or false")),
    };

    let path_val = p.get("path").filter(|v| !v.is_null());
    if let Some(v) = path_val
        && !v.is_string()
    {
        return Err(bad(cmd, "`path` must be a string"));
    }

    let is_default = units_val.is_none() && layer_val.is_none() && layers_val.is_none() && !join;

    let (text, entity_count) = if is_default {
        let mut geom = Vec::new();
        for (i, c) in sk.curves.iter().enumerate() {
            if c.construction {
                continue;
            }
            let pt = |k: usize| sk.point(k).unwrap_or_default();
            match &c.kind {
                CurveKind::Line { a, b } => geom.push(Geom2::Line(pt(*a), pt(*b))),
                CurveKind::Circle { c, r } => geom.push(Geom2::Circle(pt(*c), *r)),
                CurveKind::Arc { c, a, b } => geom.push(Geom2::Arc { c: pt(*c), a: pt(*a), b: pt(*b) }),
                _ => {
                    let poly = sk.polyline(i);
                    geom.extend(poly.windows(2).map(|w| Geom2::Line(w[0], w[1])));
                }
            }
        }
        let count = geom.len();
        (solidcraft_io::write_dxf(&geom), count)
    } else if !join {
        let hole_keys: std::collections::HashSet<Vec<&str>> =
            ss.profiles.iter().flat_map(|p| p.hole_curves.iter().map(|h| sorted_curve_ids(h))).collect();

        let mut outline_ids = std::collections::HashSet::new();
        let mut cut_ids = std::collections::HashSet::new();
        if roles_layers {
            for prof in &ss.profiles {
                for holes in &prof.hole_curves {
                    for id in holes {
                        cut_ids.insert(id.as_str());
                    }
                }
                if !hole_keys.contains(&sorted_curve_ids(&prof.outer_curves)) {
                    for id in &prof.outer_curves {
                        outline_ids.insert(id.as_str());
                    }
                }
            }
        }
        let base_layer = custom_layer.unwrap_or("0");
        let mut entities = Vec::new();
        for (i, c) in sk.curves.iter().enumerate() {
            if c.construction {
                continue;
            }
            let layer = if roles_layers {
                if cut_ids.contains(c.id.as_str()) {
                    "CUT"
                } else if outline_ids.contains(c.id.as_str()) {
                    "OUTLINE"
                } else {
                    base_layer
                }
            } else {
                base_layer
            };
            let pt = |k: usize| sk.point(k).unwrap_or_default() * scale;
            match &c.kind {
                CurveKind::Line { a, b } => entities.push((layer, Geom2::Line(pt(*a), pt(*b)))),
                CurveKind::Circle { c, r } => entities.push((layer, Geom2::Circle(pt(*c), *r * scale))),
                CurveKind::Arc { c, a, b } => entities.push((layer, Geom2::Arc { c: pt(*c), a: pt(*a), b: pt(*b) })),
                _ => {
                    let poly = sk.polyline(i);
                    for w in poly.windows(2) {
                        entities.push((layer, Geom2::Line(w[0] * scale, w[1] * scale)));
                    }
                }
            }
        }
        let count = entities.len();
        (solidcraft_io::write_dxf_entities(&entities, insunits), count)
    } else {
        // join: true -> emit closed loops as LWPOLYLINE with bulges for arcs
        let hole_keys: std::collections::HashSet<Vec<&str>> =
            ss.profiles.iter().flat_map(|p| p.hole_curves.iter().map(|h| sorted_curve_ids(h))).collect();

        let mut outline_ids = std::collections::HashSet::new();
        let mut cut_ids = std::collections::HashSet::new();
        for prof in &ss.profiles {
            for id in &prof.outer_curves {
                outline_ids.insert(id.as_str());
            }
            for holes in &prof.hole_curves {
                for id in holes {
                    cut_ids.insert(id.as_str());
                }
            }
        }
        let base_layer = custom_layer.unwrap_or("0");
        let mut entities = Vec::new();
        for prof in &ss.profiles {
            if hole_keys.contains(&sorted_curve_ids(&prof.outer_curves)) {
                continue;
            }
            let outer_layer = if roles_layers { "OUTLINE" } else { base_layer };
            let shape = solidcraft_io::loop_lwpolyline_scaled(&prof.region.outer, scale);
            entities.push(DxfEntity { layer: outer_layer.into(), shape });

            for hole in &prof.region.holes {
                let hole_layer = if roles_layers { "CUT" } else { base_layer };
                let shape = solidcraft_io::loop_lwpolyline_scaled(hole, scale);
                entities.push(DxfEntity { layer: hole_layer.into(), shape });
            }
        }
        // Preserve any non-profile open curves as disconnected entities so nothing is lost
        for (i, c) in sk.curves.iter().enumerate() {
            if c.construction || outline_ids.contains(c.id.as_str()) || cut_ids.contains(c.id.as_str()) {
                continue;
            }
            let pt = |k: usize| sk.point(k).unwrap_or_default() * scale;
            match &c.kind {
                CurveKind::Line { a, b } => {
                    entities.push(DxfEntity { layer: base_layer.into(), shape: solidcraft_io::DxfShape::Line(pt(*a), pt(*b)) })
                }
                CurveKind::Circle { c, r } => {
                    entities.push(DxfEntity { layer: base_layer.into(), shape: solidcraft_io::DxfShape::Circle { c: pt(*c), r: *r * scale } })
                }
                CurveKind::Arc { c, a, b } => {
                    let center = pt(*c);
                    let pa = pt(*a);
                    let pb = pt(*b);
                    let r = center.dist(pa);
                    let a0 = (pa - center).angle().to_degrees().rem_euclid(360.0);
                    let a1 = (pb - center).angle().to_degrees().rem_euclid(360.0);
                    entities.push(DxfEntity { layer: base_layer.into(), shape: solidcraft_io::DxfShape::Arc { c: center, r, a0, a1 } });
                }
                _ => {
                    let poly = sk.polyline(i);
                    let points: Vec<Vec2> = poly.into_iter().map(|p| p * scale).collect();
                    entities.push(DxfEntity { layer: base_layer.into(), shape: solidcraft_io::DxfShape::Polyline { points } });
                }
            }
        }
        let mut layer_names = std::collections::BTreeSet::new();
        if roles_layers {
            layer_names.insert("OUTLINE".to_string());
            layer_names.insert("CUT".to_string());
        }
        for e in &entities {
            layer_names.insert(e.layer.clone());
        }
        let layers: Vec<DxfLayer> = layer_names
            .iter()
            .map(|name| match name.as_str() {
                "OUTLINE" => DxfLayer::new(name, 7),
                "CUT" => DxfLayer::new(name, 4),
                _ => DxfLayer::new(name, 7),
            })
            .collect();
        let count = entities.len();
        (solidcraft_io::write_dxf_drawing_units(&layers, &entities, insunits)?, count)
    };

    if let Some(path) = str_(p, "path").filter(|x| !x.trim().is_empty()) {
        solidcraft_io::vfs::write(path, text.as_bytes()).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
        let exported = DxfExported::Written { path: path.to_string(), entities: entity_count };
        return reply(cmd, ExportDxfResult { exported, param_warnings: Vec::new() });
    }
    reply(cmd, ExportDxfResult { exported: DxfExported::Text { dxf: text, entities: entity_count }, param_warnings: Vec::new() })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solidcore_record::Vec2;
    use solidcraft_geom::prelude::*;
    use solidcraft_io::Geom2;
    use solidcraft_sketch::{CurveKind, Sketch};

    use crate::Session;

    #[test]
    fn dxf_and_svg_round_trip_into_sketches() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        s.execute("sketch.rectangle.two_point", &json!({"p0": [0, 0], "p1": [30, 20]})).unwrap();
        s.execute("sketch.circle.center", &json!({"center": [10, 10], "radius": 4})).unwrap();
        let out = s.execute("sketch.export_dxf", &json!({})).unwrap();
        let dxf = out["dxf"].as_str().unwrap().to_string();
        s.execute("sketch.finish", &json!({})).unwrap();
        // Insert it again as a new sketch, offset: same profiles.
        let r = s.execute("sketch.insert_dxf", &json!({"text": dxf, "at": [100, 0]})).unwrap();
        assert_eq!(r["curves"], 5, "{r}");
        let id = r["sketch"].as_u64().unwrap();
        let ss = s.model.state().sketch(id).unwrap().clone();
        let mut areas: Vec<f64> = ss.profiles.iter().map(|p| p.area).collect();
        areas.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(areas.len(), 2, "{areas:?}");
        assert!((areas[1] - (600.0 - std::f64::consts::PI * 16.0)).abs() < 1e-6, "{areas:?}");
        // SVG with a path and text, into the active sketch.
        let svg =
            r#"<svg width="40mm" height="40mm" viewBox="0 0 40 40"><path d="M0 0 H40 V40 H0 Z"/><text x="5" y="20" font-size="10">Hi</text></svg>"#;
        let r = s.execute("sketch.insert_svg", &json!({"text": svg})).unwrap();
        assert_eq!(r["curves"], 4, "{r}");
        assert_eq!(r["texts"], 1, "{r}");
        assert!(s.execute("sketch.insert_svg", &json!({"text": "<svg></svg>"})).is_err());
        assert!(s.execute("sketch.insert_dxf", &json!({"path": "/nonexistent.dxf"})).is_err());
        s.execute("sketch.finish", &json!({})).unwrap();
        // Ends that miss by 0.00005 still close the square; with a tighter tolerance they don't.
        let gappy = "0\nSECTION\n2\nENTITIES\n0\nLINE\n10\n0\n20\n0\n11\n10\n21\n0\n0\nLINE\n10\n10.00005\n20\n0\n11\n10\n21\n10\n0\nLINE\n10\n10\n20\n10\n11\n0\n21\n10\n0\nLINE\n10\n0\n20\n10.00003\n11\n0\n21\n0.00004\n0\nENDSEC\n0\nEOF\n";
        for (tol, n) in [(1e-4, 1), (1e-6, 0)] {
            let r = s.execute("sketch.insert_dxf", &json!({"text": gappy, "tolerance": tol})).unwrap();
            let ss = s.model.state().sketch(r["sketch"].as_u64().unwrap()).unwrap().clone();
            assert_eq!(ss.profiles.len(), n, "tolerance {tol}");
            s.execute("sketch.finish", &json!({})).unwrap();
        }
    }

    /// #15408: an SVG can repeat the same cubic outline backwards without losing its profile.
    #[test]
    fn repeated_svg_cubic_outlines_keep_their_curves_and_extrude() {
        let mut s = Session::default();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
            <path d="M0 0 C0 5 12 4 11 -1 C10 -6 -1 -5 0 0 Z"/>
            <path d="M0 0 C-1 -5 10 -6 11 -1 C12 4 0 5 0 0 Z"/>
        </svg>"#;
        let imported = s.execute("sketch.insert_svg", &json!({"text": svg})).unwrap();
        assert_eq!(imported["curves"], 4, "all source curves remain editable");
        let id = imported["sketch"].as_u64().unwrap();
        let areas = profile_areas(&s, id);
        assert_eq!(areas.len(), 1, "{areas:?}");
        // 1/2 ∮ (x dy - y dx) for the cubics is 1353/20 square pixels; SVG uses 96 dpi.
        let area = 1353.0 / 20.0 * (25.4_f64 / 96.0).powi(2);
        assert!((areas[0] - area).abs() < 1e-12, "{areas:?}");
        s.execute("sketch.finish", &json!({})).unwrap();
        s.execute("solid.extrude", &json!({"distance": 2})).unwrap();
        assert_eq!(s.model.state().bodies.len(), 1);
        let volume = total_volume(&s);
        assert!((volume - area * 2.0).abs() < 1e-6, "volume {volume}");
    }

    /// A degree-9 SPLINE (solidcraft#5269) with fewer control points than its degree needs still
    /// comes in as one spline over all its points, which the sketch draws at the degree they allow.
    #[test]
    fn a_degree_nine_spline_with_few_control_points_comes_in_whole() {
        let mut s = Session::default();
        for n in 2..=10 {
            let points: String = (0..n).map(|i| format!("10\n{i}\n20\n{}\n", i % 2)).collect();
            let dxf = format!("0\nSECTION\n2\nENTITIES\n0\nSPLINE\n71\n9\n{points}0\nENDSEC\n0\nEOF\n");
            let r = s.execute("sketch.insert_dxf", &json!({"text": dxf})).unwrap();
            assert_eq!(r["curves"], 1, "{n} points: {r}");
            s.execute("sketch.finish", &json!({})).unwrap();
        }
    }

    /// #11119: a closed SPLINE with more control points than a sketch spline holds keeps its
    /// whole loop, ends and all, so it still makes a profile; the tail was cut off before, which
    /// left an outline of open splines that cannot be extruded.
    #[test]
    fn a_closed_spline_with_too_many_control_points_still_closes() {
        let n = solidcraft_sketch::MAX_SPLINE_POINTS * 3;
        let mut points = String::new();
        for i in 0..=n {
            let a = std::f64::consts::TAU * (i % n) as f64 / n as f64;
            points.push_str(&format!(
                "10
{}
20
{}
",
                30.0 * a.cos(),
                30.0 * a.sin()
            ));
        }
        let dxf = format!(
            "0
SECTION
2
ENTITIES
0
SPLINE
70
11
71
3
{points}0
ENDSEC
0
EOF
"
        );
        let mut s = Session::default();
        let r = s.execute("sketch.insert_dxf", &json!({"text": dxf})).unwrap();
        let sk = s.model.state().sketch(r["sketch"].as_u64().unwrap()).unwrap().clone();
        assert_eq!(sk.profiles.len(), 1, "the loop closes into one profile: {r}");
        let want = std::f64::consts::PI * 900.0;
        assert!((sk.profiles[0].area - want).abs() < want * 0.01, "{} vs {want}", sk.profiles[0].area);
    }

    /// The sketch limit is process-wide: the tests that depend on it take turns.
    static SKETCH_LIMIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #10874: a drawing with more entities than a sketch can hold is refused with the point
    /// cap and that scale alone does not help (a floorplan SVG often hits this).
    #[test]
    fn a_drawing_with_too_many_entities_names_the_sketch_cap() {
        let _turn = SKETCH_LIMIT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = solidcraft_sketch::MAX_POINTS + 1;
        let mut dxf = String::from("0\nSECTION\n2\nENTITIES\n");
        for i in 0..n {
            dxf.push_str(&format!("0\nLINE\n10\n{i}\n20\n0\n11\n{}\n21\n1\n", i + 1));
        }
        dxf.push_str("0\nENDSEC\n0\nEOF\n");
        let mut s = Session::default();
        let e = s.execute("sketch.insert_dxf", &json!({"text": dxf})).unwrap_err().to_string();
        assert!(e.contains("20000") && e.contains("scale") && e.contains("Largest sketch"), "{e}");
        assert!(s.active_sketch.is_none(), "a refused insert leaves no empty sketch behind");
    }

    /// #10874: with Preferences › Sketch › Largest sketch raised, a drawing past the default
    /// limit goes into one sketch; back at the default it is refused again.
    #[test]
    fn a_raised_sketch_limit_takes_a_denser_drawing() {
        use solidcraft_sketch::limits;
        let _turn = SKETCH_LIMIT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = limits::DEFAULT_POINTS + 1;
        let mut dxf = String::from("0\nSECTION\n2\nENTITIES\n");
        for i in 0..n {
            dxf.push_str(&format!("0\nPOINT\n10\n{}\n20\n{}\n", i % 200, i / 200));
        }
        dxf.push_str("0\nENDSEC\n0\nEOF\n");
        let mut s = Session::default();
        limits::set_max_points(limits::DEFAULT_POINTS + 1000).unwrap();
        let raised = s.execute("sketch.insert_dxf", &json!({"text": dxf}));
        limits::set_max_points(limits::DEFAULT_POINTS).unwrap();
        raised.unwrap();
        let id = s.active_sketch.unwrap();
        assert!(super::sketch_store::read(&s.doc, id).unwrap().points.len() > limits::DEFAULT_POINTS, "every point went in");
        s.execute("sketch.finish", &json!({})).unwrap();
        let e = s.execute("sketch.insert_dxf", &json!({"text": dxf})).unwrap_err().to_string();
        assert!(e.contains("at most 20000"), "{e}");
    }

    /// #9641: an SVG with nothing SolidCraft can draw (here only an embedded picture) is refused
    /// with a message naming the file and saying why, not "invalid parameters … no geometry"; a
    /// UTF-16 SVG file inserts.
    #[test]
    fn an_svg_with_nothing_to_import_says_why() {
        let dir = std::env::temp_dir().join(format!("solidcraft-9641-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("logo.svg");
        std::fs::write(
            &picture,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><image width="64" height="64" href="data:image/png;base64,iVBORw0KGgo="/></svg>"#,
        )
        .unwrap();
        let mut s = Session::default();
        for cmd in ["sketch.insert_svg", "sketch.drawing_bounds"] {
            let e = s.execute(cmd, &json!({"path": picture.to_string_lossy(), "align": "center"})).unwrap_err().to_string();
            assert!(e.starts_with("logo.svg: ") && e.contains("embedded picture") && !e.contains("invalid parameters"), "{cmd}: {e}");
        }
        assert!(s.active_sketch.is_none(), "a refused insert leaves no empty sketch behind");
        let square = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>"#;
        let utf16 = dir.join("square.svg");
        std::fs::write(&utf16, [0xFF, 0xFE].into_iter().chain(square.encode_utf16().flat_map(u16::to_le_bytes)).collect::<Vec<u8>>()).unwrap();
        let r = s.execute("sketch.insert_svg", &json!({"path": utf16.to_string_lossy()})).unwrap();
        assert_eq!(r["curves"], 4, "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #8155: an elliptical arc of a DXF (as Fusion writes one) comes into the sketch as exact
    /// conics, and the drawing's size is the arc's own, not the box round the conics' apexes.
    #[test]
    fn an_elliptical_arc_comes_in_as_conics_at_its_own_size() {
        // Centre (0, 0), major radius 10 along X, minor 5, from -45° to 45°: one quarter turn,
        // whose apex is at x = 10 / cos 45° = 14.1, past the arc's end at x = 10.
        let dxf = format!(
            "0\nSECTION\n2\nENTITIES\n0\nELLIPSE\n10\n0\n20\n0\n11\n10\n21\n0\n40\n0.5\n41\n{}\n42\n{}\n0\nENDSEC\n0\nEOF\n",
            -std::f64::consts::FRAC_PI_4,
            std::f64::consts::FRAC_PI_4
        );
        let mut s = Session::default();
        let b = s.execute("sketch.drawing_bounds", &json!({"text": dxf, "format": "dxf"})).unwrap();
        let (w, h) = (b["width"].as_f64().unwrap(), b["height"].as_f64().unwrap());
        let half = std::f64::consts::FRAC_1_SQRT_2;
        assert!((w - 10.0 * (1.0 - half)).abs() < 1e-3 && (h - 10.0 * half).abs() < 1e-3, "{b}");
        let r = s.execute("sketch.insert_dxf", &json!({"text": dxf})).unwrap();
        assert_eq!(r["curves"], 1, "{r}");
        let st = s.model.state();
        let sk = &st.sketch(r["sketch"].as_u64().unwrap()).unwrap().sketch;
        assert!(matches!(sk.curves[0].kind, solidcraft_sketch::CurveKind::Conic { .. }), "{:?}", sk.curves[0].kind);
        // Every point the sketch draws it with lies on the ellipse.
        let worst = sk.polyline(0).iter().map(|p| ((p.x / 10.0).powi(2) + (p.y / 5.0).powi(2) - 1.0).abs()).fold(0.0, f64::max);
        assert!(worst < 1e-9, "{worst}");
    }

    /// What an insert's reply warns of (it leaves `warnings` out when there is nothing).
    fn warns(r: &serde_json::Value) -> &[serde_json::Value] {
        r["warnings"].as_array().map_or(&[], Vec::as_slice)
    }

    /// A degree-`p` SPLINE over `ctrl` with `knots` and, when given, `weights`.
    fn spline_dxf(p: usize, ctrl: &[[f64; 2]], knots: &[f64], weights: &[f64]) -> String {
        let mut body = format!("0\nSECTION\n2\nENTITIES\n0\nSPLINE\n71\n{p}\n");
        body.extend(knots.iter().map(|k| format!("40\n{k}\n")));
        body.extend(weights.iter().map(|w| format!("41\n{w}\n")));
        body.extend(ctrl.iter().map(|[x, y]| format!("10\n{x}\n20\n{y}\n")));
        body + "0\nENDSEC\n0\nEOF\n"
    }

    /// De Boor's evaluation, in homogeneous coordinates when there are weights, both factors of
    /// each step from knot differences: the B-spline at `u`, independent of the importer.
    fn de_boor(ctrl: &[[f64; 2]], weights: &[f64], knots: &[f64], p: usize, u: f64) -> Vec2 {
        let k = (p..ctrl.len()).rev().find(|&k| knots[k] <= u && knots[k] < knots[k + 1]).unwrap();
        let w = |i: usize| if weights.is_empty() { 1.0 } else { weights[i] };
        let mut d: Vec<[f64; 3]> = (0..=p).map(|j| j + k - p).map(|i| [ctrl[i][0] * w(i), ctrl[i][1] * w(i), w(i)]).collect();
        for r in 1..=p {
            for j in (r..=p).rev() {
                let i = j + k - p;
                let (lo, hi) = (knots[i], knots[i + p + 1 - r]);
                let (f, g) = ((hi - u) / (hi - lo), (u - lo) / (hi - lo));
                d[j] = [0, 1, 2].map(|c| d[j - 1][c] * f + d[j][c] * g);
            }
        }
        Vec2::new(d[p][0] / d[p][2], d[p][1] / d[p][2])
    }

    fn insert_scaled(dxf: &str, scale: f64) -> Result<(serde_json::Value, Sketch, usize), String> {
        insert_with(json!({"text": dxf, "scale": scale}))
    }

    fn insert_with(params: serde_json::Value) -> Result<(serde_json::Value, Sketch, usize), String> {
        let mut s = Session::default();
        let r = s.execute("sketch.insert_dxf", &params).map_err(|e| e.to_string())?;
        let ss = s.model.state().sketch(r["sketch"].as_u64().unwrap()).unwrap().clone();
        Ok((r, ss.sketch, ss.profiles.len()))
    }

    fn insert_one(dxf: &str) -> (serde_json::Value, Sketch, usize) {
        insert_scaled(dxf, 1.0).unwrap()
    }

    /// Sketch curve `i` at `t ∈ [0, 1]`.
    fn curve_point(sk: &Sketch, i: usize, t: f64) -> Vec2 {
        let at = |j: usize| sk.point(j).unwrap();
        match &sk.curves[i].kind {
            CurveKind::Line { a, b } => at(*a) + (at(*b) - at(*a)) * t,
            CurveKind::Spline { pts, control, degree, periodic, nurbs, .. } => {
                solidcraft_sketch::spline_point_mode(&pts.iter().map(|j| at(*j)).collect::<Vec<_>>(), *control, *degree, *periodic, nurbs.as_ref(), t)
            }
            CurveKind::Conic { a, b, apex, rho } => solidcraft_sketch::conic_point(at(*a), at(*apex), at(*b), *rho, t),
            k => panic!("{k:?}"),
        }
    }

    /// The distance from `x` to the curve `f` over `[lo, hi]`: samples spread along the curve, not
    /// its parameter (a rational curve can run much of its length in a sliver of it), and closer
    /// together the nearer they come to `x` (a curve can loop tighter than the spacing), each one
    /// closer than its neighbours refined by golden-section search. Never less than the true
    /// distance (each is to a point on the curve), so a check against it can't pass wrongly.
    fn closest(f: &dyn Fn(f64) -> Vec2, lo: f64, hi: f64, x: Vec2) -> f64 {
        let mut s: Vec<f64> = (0..=64).map(|i| lo + (hi - lo) * f64::from(i) / 64.0).collect();
        let mut p: Vec<Vec2> = s.iter().map(|&t| f(t)).collect();
        let length: f64 = p.windows(2).map(|w| w[0].dist(w[1])).sum();
        for _ in 0..60 {
            let (mut ns, mut np) = (vec![s[0]], vec![p[0]]);
            for i in 1..s.len() {
                let near = 0.5 * p[i - 1].dist(x).min(p[i].dist(x));
                if p[i - 1].dist(p[i]) > near.min(length / 512.0) && s[i] - s[i - 1] > 1e-13 * (hi - lo) && ns.len() < 20_000 {
                    let mid = 0.5 * (s[i - 1] + s[i]);
                    ns.push(mid);
                    np.push(f(mid));
                }
                ns.push(s[i]);
                np.push(p[i]);
            }
            if ns.len() == s.len() {
                break;
            }
            (s, p) = (ns, np);
        }
        let d: Vec<f64> = p.iter().map(|q| q.dist(x)).collect();
        let last = d.len() - 1;
        let mut found = d.iter().copied().fold(f64::INFINITY, f64::min);
        for i in (0..=last).filter(|&i| d[i] <= d[i.saturating_sub(1)] && d[i] <= d[(i + 1).min(last)]) {
            let (mut a, mut b) = (s[i.saturating_sub(1)], s[(i + 1).min(last)]);
            for _ in 0..80 {
                let (m1, m2) = (a + (b - a) * 0.381_966, b - (b - a) * 0.381_966);
                let (d1, d2) = (f(m1).dist(x), f(m2).dist(x));
                found = found.min(d1).min(d2);
                if d1 < d2 { b = m2 } else { a = m1 }
            }
        }
        found
    }

    /// Every point of the file's curve (scaled by `k`) within `allowed` of the sketch's curves from
    /// `from` on, and the points of the first, middle and last of those within it of the file's
    /// curve.
    fn assert_within(sk: &Sketch, from: usize, ctrl: &[[f64; 2]], weights: &[f64], knots: &[f64], p: usize, k: f64, allowed: f64) {
        let (lo, hi) = (knots[p], knots[ctrl.len()]);
        let file = |u: f64| de_boor(ctrl, weights, knots, p, u) * k;
        for j in 0..=40 {
            let x = file(lo + (hi - lo) * f64::from(j) / 40.0);
            let off = (from..sk.curves.len()).map(|i| closest(&|t| curve_point(sk, i, t), 0.0, 1.0, x)).fold(f64::INFINITY, f64::min);
            assert!(off <= allowed, "file point {j}/40 is {off:e} from the sketch's curves");
        }
        let last = sk.curves.len() - 1;
        for i in [from, (from + last) / 2, last] {
            for j in 0..=4 {
                let y = curve_point(sk, i, f64::from(j) / 4.0);
                let off = closest(&file, lo, hi, y);
                assert!(off <= allowed, "curve {i} at {j}/4 is {off:e} from the file's curve");
            }
        }
    }

    /// #9355: a SPLINE with uneven knots or a double knot comes in as one sketch spline over its
    /// points that keeps those knots (the sketch's own knotted spline, [`Nurbs`]), on the file's
    /// curve (by de Boor) all along, not a spline over the points with even knots. One with
    /// unclamped closed knots, which a sketch spline can't have, comes in as one control-point
    /// spline per span of its domain, each on the file's curve.
    #[test]
    fn a_spline_comes_in_as_its_own_knots_describe() {
        let open = [[0.0, 0.0], [10.0, 25.0], [30.0, 30.0], [45.0, 5.0], [60.0, 20.0], [80.0, 0.0]];
        let (a, b, c) = ([0.0, 0.0], [20.0, 0.0], [10.0, 20.0]);
        let closed = [a, b, c, a, b, c];
        for (ctrl, knots, closes) in [
            (&open[..], vec![0.0, 0.0, 0.0, 0.0, 0.1, 0.15, 1.0, 1.0, 1.0, 1.0], false),
            (&open, vec![0.0, 0.0, 0.0, 0.0, 0.4, 0.4, 1.0, 1.0, 1.0, 1.0], false),
            (&closed, (0..10).map(f64::from).collect(), true),
        ] {
            let (r, sk, profiles) = insert_one(&spline_dxf(3, ctrl, &knots, &[]));
            assert!(warns(&r).is_empty(), "{r}");
            let spans: Vec<(f64, f64)> = knots[3..=ctrl.len()].windows(2).map(|w| (w[0], w[1])).filter(|(a, b)| a < b).collect();
            if closes {
                assert_eq!(sk.curves.len(), spans.len(), "{knots:?}");
            } else {
                // One spline with the file's knots, over its six points.
                let [curve] = &sk.curves[..] else { panic!("{knots:?}: {r}") };
                let CurveKind::Spline { pts, control: true, degree: 3, periodic: false, nurbs: Some(n), .. } = &curve.kind else {
                    panic!("{:?}", curve.kind)
                };
                assert!(pts.len() == ctrl.len() && n.knots == knots && n.weights.is_empty(), "{knots:?}: {n:?}");
            }
            for (i, (lo, hi)) in spans.into_iter().enumerate().filter(|_| closes) {
                let CurveKind::Spline { pts, control: true, degree: 3, .. } = &sk.curves[i].kind else { panic!("{:?}", sk.curves[i].kind) };
                // A cubic's piece: four control points, repeated ones included.
                assert_eq!(pts.len(), 4, "{knots:?}: {pts:?}");
                for j in 0..=16 {
                    let t = f64::from(j) / 16.0;
                    let (got, want) = (curve_point(&sk, i, t), de_boor(ctrl, &[], &knots, 3, lo + (hi - lo) * t));
                    assert!(got.dist(want) < 1e-9, "{knots:?} at {t} of [{lo}, {hi}]: {got:?} vs {want:?}");
                }
            }
            if !closes {
                let (lo, hi) = (knots[3], knots[ctrl.len()]);
                for j in 0..=32 {
                    let t = f64::from(j) / 32.0;
                    let (got, want) = (curve_point(&sk, 0, t), de_boor(ctrl, &[], &knots, 3, lo + (hi - lo) * t));
                    assert!(got.dist(want) < 1e-9, "{knots:?} at {t}: {got:?} vs {want:?}");
                }
            }
            assert_eq!(profiles, usize::from(closes), "{knots:?}");
        }
    }

    /// Whether sketch spline `i` repeats a control point, which the sketch's own spline tool
    /// refuses.
    fn repeats_a_point(sk: &Sketch, i: usize) -> bool {
        let CurveKind::Spline { pts, .. } = &sk.curves[i].kind else { return false };
        pts.windows(2).any(|w| sk.point(w[0]).unwrap().dist(sk.point(w[1]).unwrap()) < solidcraft_geom::bspline::MIN_CONTROL_GAP)
    }

    /// A spline repeating a control point (a Bézier [A, A, B, C], one with the zero-length handle
    /// at its end, an exact piece starting so) has an end with no direction, which a sketch spline
    /// can't have and the solid kernel won't build on: it comes in as curves that repeat no point,
    /// within 0.001 mm of the file's curve, and the import says so. An SVG path's zero-length handle
    /// (`C4,31 8,20 8,20`) alike, and a handle that only repeats a point once the drawing is
    /// scaled down (3·10⁻⁸ in the file, 3·10⁻¹⁰ mm in the sketch at a hundredth): the gap is kept
    /// in the sketch. (The SVG reader splits a cubic its own way first, #16190: a cubic whose
    /// points lie on its chord is a line, and one whose handle is that short in the file comes in
    /// as pieces that repeat no point, so the import has nothing to say about it.)
    #[test]
    fn a_repeated_control_point_comes_in_as_curves_that_dont_repeat_it() {
        let (a, b, c) = ([0.0, 0.0], [20.0, 0.0], [10.0, 20.0]);
        let cubic = vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0];
        for (ctrl, knots) in [
            (vec![a, a, b, c], cubic.clone()),
            (vec![a, b, c, c], cubic),
            (vec![a, a, b, c, [30.0, 5.0]], vec![0.0, 0.0, 0.0, 0.0, 0.3, 1.0, 1.0, 1.0, 1.0]),
        ] {
            let (r, sk, _) = insert_one(&spline_dxf(3, &ctrl, &knots, &[]));
            let warned = warns(&r).iter().any(|w| w.as_str().unwrap().starts_with("1 spline(s) repeat a control point"));
            assert!(warned, "{r}");
            assert!((0..sk.curves.len()).all(|i| !repeats_a_point(&sk, i)), "{ctrl:?}: {:?}", sk.curves);
            assert_within(&sk, 0, &ctrl, &[], &knots, 3, 1.0, 1e-3);
        }
        for (path, scale, says) in [("M5,35 C4,31 8,20 8,20", 1.0, true), ("M5,35 C5.00000003,35 8,21 8,20", 0.01, false)] {
            let svg = format!(r#"<svg width="40mm" height="40mm" viewBox="0 0 40 40"><path d="{path}"/></svg>"#);
            let mut s = Session::default();
            let r = s.execute("sketch.insert_svg", &json!({"text": svg, "scale": scale})).unwrap();
            let sk = s.model.state().sketch(r["sketch"].as_u64().unwrap()).unwrap().sketch.clone();
            let warned = warns(&r).iter().any(|w| w.as_str().unwrap().starts_with("1 spline(s) repeat a control point"));
            assert!(
                warned == says && !sk.curves.is_empty() && (0..sk.curves.len()).all(|i| !repeats_a_point(&sk, i)),
                "{path} at {scale}: {r} {:?}",
                sk.curves
            );
        }
    }

    /// A spline whose control points all lie within the sketch's gap of one another is a point,
    /// not a curve: it is left out, and the import says so, rather than coming in as a closed
    /// spline smaller than the gap. The gap is in the sketch: a spline its own at full size is left
    /// out of a drawing scaled to a hundredth.
    #[test]
    fn a_spline_smaller_than_the_gap_is_left_out() {
        let knots = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let tiny = spline_dxf(2, &[[0.0, 0.0], [1e-10, 0.0], [2e-10, 0.0]], &knots, &[]);
        let small = spline_dxf(2, &[[0.0, 0.0], [1e-8, 0.0], [2e-8, 0.0]], &knots, &[]);
        for (dxf, scale) in [(&tiny, 1.0), (&small, 0.01)] {
            let (r, sk, _) = insert_scaled(dxf, scale).unwrap();
            let warnings: Vec<&str> = warns(&r).iter().filter_map(|w| w.as_str()).collect();
            assert!(sk.curves.is_empty() && warnings.len() == 1 && warnings[0].starts_with("1 spline(s) left out"), "{r}");
        }
        let (r, sk, _) = insert_one(&small);
        assert_eq!((sk.curves.len(), warns(&r).len()), (1, 0), "{r}");
    }

    /// `fit_tolerance` sets how close a spline with no exact sketch curve comes in, apart from the
    /// join `tolerance`: looser takes fewer curves. Outside 0.0001 to 10 mm it is refused.
    #[test]
    fn the_fit_tolerance_sets_how_close_a_spline_comes_in() {
        let (ctrl, weights, knots) =
            ([[0.0, 0.0], [5.0, 30.0], [40.0, 30.0], [50.0, 0.0]], [1.0, 3.0, 0.5, 1.0], [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        let dxf = spline_dxf(3, &ctrl, &knots, &weights);
        let curves = |fit: f64| {
            let (r, sk, _) = insert_with(json!({"text": dxf, "fit_tolerance": fit})).unwrap();
            assert_within(&sk, 0, &ctrl, &weights, &knots, 3, 1.0, fit);
            assert!(warns(&r).iter().all(|w| !w.as_str().unwrap().contains("couldn't be followed")), "{r}");
            sk.curves.len()
        };
        let (loose, default, tight) = (curves(0.1), curves(0.001), curves(0.0001));
        assert!(loose < default && default < tight, "{loose} {default} {tight}");
        for bad in [json!(0.0), json!(-1.0), json!(1e-5), json!(11.0), json!("0.01")] {
            let e = insert_with(json!({"text": dxf, "fit_tolerance": bad})).unwrap_err();
            assert!(e.contains("`fit_tolerance` must be a number of mm from 0.0001 to 10"), "{bad}: {e}");
        }
    }

    /// A spline that can't be followed within the fit tolerance in the curves a piece may take (a
    /// 1000 km curve with weights 10⁸ apart) comes in within a looser tolerance, and the import
    /// gives the proven distance; it is not refused.
    #[test]
    fn a_spline_that_cant_be_followed_closely_comes_in_and_says_how_close() {
        let (ctrl, weights, knots) =
            ([[0.0, 0.0], [0.0, 1e9], [1e9, 3e8], [2e8, 0.0]], [1.0, 1e4, 1e-4, 1.0], [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        let (r, sk, _) = insert_one(&spline_dxf(3, &ctrl, &knots, &weights));
        let warning = warns(&r).iter().filter_map(|w| w.as_str()).find(|w| w.contains("couldn't be followed")).unwrap_or_else(|| panic!("{r}"));
        let close: f64 = warning
            .split("come in followed within ")
            .nth(1)
            .and_then(|w| w.split(' ').next())
            .and_then(|x| x.parse().ok())
            .unwrap_or_else(|| panic!("{warning}"));
        assert!(close > 5e-4, "{warning}");
        assert_within(&sk, 0, &ctrl, &weights, &knots, 3, 1.0, close * 1.1);
    }

    /// A SPLINE that is the sketch's own (clamped, evenly spaced knots, no weights or all alike)
    /// comes in as one editable spline over its points with no knots of its own, as it did before;
    /// knots written to six digits are a different curve and come in as one spline that keeps
    /// them. Weights that differ come in as the exact conics, or followed within the tolerance.
    #[test]
    fn the_sketchs_own_spline_comes_in_as_one_spline() {
        let five = [[0.0, 0.0], [10.0, 20.0], [30.0, 20.0], [40.0, 0.0], [60.0, 10.0]];
        let even = [0.0, 0.0, 0.0, 0.0, 0.5, 1.0, 1.0, 1.0, 1.0];
        for weights in [&[][..], &[2.0; 5]] {
            let (r, sk, _) = insert_one(&spline_dxf(3, &five, &even, weights));
            let [curve] = &sk.curves[..] else { panic!("{r}") };
            let CurveKind::Spline { pts, control: true, degree: 3, .. } = &curve.kind else { panic!("{:?}", curve.kind) };
            let pts: Vec<Vec2> = pts.iter().map(|i| sk.point(*i).unwrap()).collect();
            assert!(pts.iter().zip(&five).all(|(p, q)| p.dist(Vec2::new(q[0], q[1])) < 1e-12), "{pts:?}");
        }
        let six = [[0.0, 0.0], [10.0, 20.0], [30.0, 20.0], [40.0, 0.0], [60.0, 10.0], [70.0, 5.0]];
        let thirds = [0.0, 0.0, 0.0, 0.0, 0.333333, 0.666667, 1.0, 1.0, 1.0, 1.0];
        let (_, sk, _) = insert_one(&spline_dxf(3, &six, &thirds, &[]));
        let [curve] = &sk.curves[..] else { panic!("{:?}", sk.curves) };
        assert!(matches!(&curve.kind, CurveKind::Spline { nurbs: Some(n), .. } if n.knots == thirds), "{:?}", curve.kind);
        assert_within(&sk, 0, &six, &[], &thirds, 3, 1.0, 1e-9);
    }

    /// #9355: a SPLINE with more control points than a sketch spline holds (500) comes in whole,
    /// as exact pieces on the file's curve from its first point to its last: with uneven knots,
    /// with even ones, and with none given (the sketch's even knots).
    #[test]
    fn a_spline_past_five_hundred_control_points_comes_in_whole() {
        let n = 520;
        let ctrl: Vec<[f64; 2]> = (0..n).map(|i| [i as f64, 10.0 * (i as f64 / 7.0).sin()]).collect();
        let even: Vec<f64> = (0..n + 4).map(|i| (i.clamp(3, n) - 3) as f64 / (n - 3) as f64).collect();
        let uneven: Vec<f64> = (0..n + 4).map(|i| ((i.clamp(3, n) - 3) as f64 / (n - 3) as f64).powi(2)).collect();
        for (given, knots) in [(&uneven[..], &uneven), (&even, &even), (&[][..], &even)] {
            let (r, sk, _) = insert_one(&spline_dxf(3, &ctrl, given, &[]));
            assert_eq!((r["curves"].as_u64(), warns(&r).len()), (Some(n as u64 - 3), 0), "{} knots given", given.len());
            for i in 0..sk.curves.len() {
                assert!(matches!(sk.curves[i].kind, CurveKind::Spline { control: true, degree: 3, .. }), "{:?}", sk.curves[i].kind);
                let (lo, hi) = (knots[i + 3], knots[i + 4]);
                for j in 0..=4 {
                    let t = f64::from(j) / 4.0;
                    let (got, want) = (curve_point(&sk, i, t), de_boor(&ctrl, &[], knots, 3, lo + (hi - lo) * t));
                    assert!(got.dist(want) < 1e-9, "{} knots given, piece {i} at {t}: {got:?} vs {want:?}", given.len());
                }
            }
        }
    }

    /// #9355: weighted quadratics (how CAD programs write arcs and circles) come in as conics on
    /// the circle all along: a quarter, and a whole circle of four, closed.
    #[test]
    fn a_weighted_quadratic_spline_comes_in_as_conics_on_its_circle() {
        let h = std::f64::consts::FRAC_1_SQRT_2;
        let quarter = spline_dxf(2, &[[20.0, 0.0], [20.0, 20.0], [0.0, 20.0]], &[0.0, 0.0, 0.0, 1.0, 1.0, 1.0], &[1.0, h, 1.0]);
        let ctrl = [[20.0, 0.0], [20.0, 20.0], [0.0, 20.0], [-20.0, 20.0], [-20.0, 0.0], [-20.0, -20.0], [0.0, -20.0], [20.0, -20.0], [20.0, 0.0]];
        let knots = [0.0, 0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0, 1.0];
        let circle = spline_dxf(2, &ctrl, &knots, &[1.0, h, 1.0, h, 1.0, h, 1.0, h, 1.0]);
        for (dxf, conics, profiles) in [(quarter, 1, 0), (circle, 4, 1)] {
            let (r, sk, found) = insert_one(&dxf);
            assert_eq!((r["curves"].as_u64(), found, warns(&r).len()), (Some(conics), profiles, 0), "{r}");
            for i in 0..sk.curves.len() {
                assert!(matches!(sk.curves[i].kind, CurveKind::Conic { .. }), "{:?}", sk.curves[i].kind);
                let worst = (0..=64).map(|j| (curve_point(&sk, i, f64::from(j) / 64.0).len() - 20.0).abs()).fold(0.0, f64::max);
                assert!(worst < 1e-9, "{worst}");
            }
        }
    }

    /// #9355: a weighted cubic has no exact sketch curve: it comes in as cubic splines within
    /// 0.001 mm of it in the sketch, scaled up or not, and the import says how close.
    #[test]
    fn a_weighted_cubic_spline_comes_in_within_its_tolerance_and_the_import_says_so() {
        let (ctrl, weights, knots) =
            ([[0.0, 0.0], [5.0, 30.0], [40.0, 30.0], [50.0, 0.0]], [1.0, 3.0, 0.5, 1.0], [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        for scale in [1.0, 40.0] {
            let (r, sk, _) = insert_scaled(&spline_dxf(3, &ctrl, &knots, &weights), scale).unwrap();
            assert!(sk.curves.iter().all(|c| matches!(c.kind, CurveKind::Spline { control: true, degree: 3, .. })), "{r}");
            let warnings = warns(&r);
            assert!(warnings.len() == 1 && warnings[0].as_str().unwrap().starts_with("1 spline piece(s) have no exact sketch curve"), "{r}");
            assert_within(&sk, 0, &ctrl, &weights, &knots, 3, scale, 1e-3);
        }
    }

    /// A weighted quadratic so sharp (middle weight 2·10⁹, so 1 − rho below 10⁻⁹) that the sketch
    /// would hold its conic at another weight comes in as cubics within 0.001 mm, not as that conic.
    #[test]
    fn a_quadratic_sharper_than_a_sketch_conic_comes_in_as_cubics() {
        let (ctrl, weights, knots) = ([[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0]], [1.0, 2e9, 1.0], [0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
        let (r, sk, _) = insert_one(&spline_dxf(2, &ctrl, &knots, &weights));
        assert!(sk.curves.iter().all(|c| matches!(c.kind, CurveKind::Spline { control: true, degree: 3, .. })), "{r}");
        assert_within(&sk, 0, &ctrl, &weights, &knots, 2, 1.0, 1e-3);
    }

    /// A spline's end joining a line's end 0.01 mm away, with a join tolerance of 0.05 mm, moves
    /// that far: the import says so, for a weighted cubic split into pieces and for the sketch's
    /// own spline alike. With the default tolerance it doesn't join, and the spline keeps within
    /// 0.001 mm.
    #[test]
    fn a_spline_end_moved_further_than_the_tolerance_to_join_is_reported() {
        let (ctrl, knots) = ([[0.0, 0.0], [5.0, 30.0], [40.0, 30.0], [50.0, 0.0]], [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        let line = "0\nSECTION\n2\nENTITIES\n0\nLINE\n10\n-20\n20\n0\n11\n0.01\n21\n0\n";
        for weights in [&[1.0, 3.0, 0.5, 1.0][..], &[]] {
            let dxf = line.to_string() + spline_dxf(3, &ctrl, &knots, weights).trim_start_matches("0\nSECTION\n2\nENTITIES\n");
            let (r, sk, _) = insert_with(json!({"text": dxf, "tolerance": 0.05})).unwrap();
            let warned = |r: &serde_json::Value, start: &str| warns(r).iter().filter(|w| w.as_str().unwrap().starts_with(start)).count();
            assert_eq!(warned(&r, "curve ends moved up to 1.0e-2 mm"), 1, "{weights:?}: {r}");
            let (r, sk2, _) = insert_with(json!({"text": dxf})).unwrap();
            assert_eq!(warned(&r, "curve ends moved"), 0, "{weights:?}: {r}");
            assert_within(&sk2, 1, &ctrl, weights, &knots, 3, 1.0, 1e-3);
            assert!(sk.curves.len() == sk2.curves.len());
        }
    }

    /// An inner control point of a spline joins no other curve, even one ending right on it: it
    /// shapes the curve, and moving it would move the curve. The ends still join.
    #[test]
    fn an_inner_spline_point_joins_no_other_curve() {
        let dxf = "0\nSECTION\n2\nENTITIES\n0\nLINE\n10\n100\n20\n100\n11\n10\n21\n25\n0\nLINE\n10\n45\n20\n5\n11\n90\n21\n5\n\
                   0\nSPLINE\n71\n3\n10\n0\n20\n0\n10\n10\n20\n25\n10\n30\n20\n30\n10\n45\n20\n5\n0\nENDSEC\n0\nEOF\n";
        let (_, sk, _) = insert_one(dxf);
        let (CurveKind::Line { b: inner_end, .. }, CurveKind::Line { a: end, .. }, CurveKind::Spline { pts, .. }) =
            (&sk.curves[0].kind, &sk.curves[1].kind, &sk.curves[2].kind)
        else {
            panic!("{:?}", sk.curves)
        };
        assert!(pts[1] != *inner_end && pts[3] == *end, "{pts:?}");
    }

    /// Random SPLINEs of degree 1 to 9 (now and then 11), clamped or not, inner knots repeated up
    /// to and past the degree, weights up to 100 apart, inserted at three scales: each comes in
    /// within 0.001 mm of the file's curve in the sketch, both ways, or is refused with the reason.
    #[test]
    fn random_splines_come_in_within_the_tolerance_or_are_refused() {
        let mut rng = 0x9355_u64;
        let mut unit = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let (mut came, mut refused) = (0, 0);
        for _ in 0..150 {
            let p = if unit() < 0.1 { 11 } else { 1 + (unit() * 9.0) as usize };
            let n = p + 1 + (unit() * 8.0) as usize;
            let clamped = unit() < 0.6;
            let mut knots = Vec::with_capacity(n + p + 1);
            let mut u = 0.0;
            for i in 0..n + p + 1 {
                if i > 0 && !(clamped && (i <= p || i > n)) && !(i > p && i < n && unit() < 0.25) {
                    u += 0.05 + unit();
                }
                knots.push(u);
            }
            let ctrl: Vec<[f64; 2]> = (0..n).map(|_| [100.0 * unit() - 50.0, 100.0 * unit() - 50.0]).collect();
            let weights: Vec<f64> = if unit() < 0.5 { Vec::new() } else { (0..n).map(|_| 10f64.powf(2.0 * unit() - 1.0)).collect() };
            let scale = [1.0, 7.5, 0.02][(unit() * 3.0) as usize];
            match insert_scaled(&spline_dxf(p, &ctrl, &knots, &weights), scale) {
                Ok((_, sk, _)) => {
                    assert_within(&sk, 0, &ctrl, &weights, &knots, p, scale, 1e-3 * (1.0 + 1e-6) + 1e-12 * 100.0 * scale);
                    came += 1;
                }
                Err(e) => {
                    assert!(e.contains("spline") || e.contains("SPLINE"), "{e}");
                    refused += 1;
                }
            }
        }
        assert!(came > 100 && refused > 5, "came in {came}, refused {refused}");
    }

    const PLATE: &str =
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="10mm" viewBox="0 0 20 10"><rect x="0" y="0" width="20" height="10"/></svg>"#;

    fn span(s: &Session) -> (f64, f64, f64, f64) {
        let st = s.model.state();
        let sk = &st.sketch(s.active_sketch.unwrap()).unwrap().sketch;
        let (mut lo, mut hi) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
        for p in (0..sk.curves.len()).flat_map(|i| sk.polyline(i)) {
            lo = (lo.0.min(p.x), lo.1.min(p.y));
            hi = (hi.0.max(p.x), hi.1.max(p.y));
        }
        (lo.0, lo.1, hi.0, hi.1)
    }

    fn inserted(params: serde_json::Value) -> (f64, f64, f64, f64) {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let mut p = json!({"text": PLATE});
        p.as_object_mut().unwrap().extend(params.as_object().unwrap().clone());
        s.execute("sketch.insert_svg", &p).unwrap();
        span(&s)
    }

    /// A drawing can be given a size in millimetres (its aspect kept), turned about its origin and
    /// placed; the size of the file itself is read without inserting anything.
    #[test]
    fn an_inserted_drawing_takes_a_width_a_height_and_an_angle() {
        let mut s = Session::default();
        let b = s.execute("sketch.drawing_bounds", &json!({"text": PLATE, "format": "svg"})).unwrap();
        assert_eq!((b["width"].as_f64(), b["height"].as_f64()), (Some(20.0), Some(10.0)));
        let near = |a: (f64, f64, f64, f64), e: (f64, f64, f64, f64)| {
            assert!((a.0 - e.0).abs() < 1e-6 && (a.1 - e.1).abs() < 1e-6 && (a.2 - e.2).abs() < 1e-6 && (a.3 - e.3).abs() < 1e-6, "{a:?} vs {e:?}")
        };
        near(inserted(json!({"width": 40})), (0.0, -20.0, 40.0, 0.0));
        near(inserted(json!({"height": 5, "at": [3, 4]})), (3.0, -1.0, 13.0, 4.0));
        near(inserted(json!({"width": 40, "scale": 9})), (0.0, -20.0, 40.0, 0.0));
        // Turned 90 degrees about the origin, then placed.
        near(inserted(json!({"angle": 90, "at": [100, 0]})), (100.0, 0.0, 110.0, 20.0));
        // About its centre: the middle lands on `at` whatever the size and angle.
        near(inserted(json!({"align": "center", "at": [50, 50]})), (40.0, 45.0, 60.0, 55.0));
        near(inserted(json!({"align": "center", "at": [50, 50], "width": 40})), (30.0, 40.0, 70.0, 60.0));
        near(inserted(json!({"align": "center", "at": [50, 50], "angle": 90})), (45.0, 40.0, 55.0, 60.0));
        for bad in [json!({"width": 0}), json!({"height": -1}), json!({"angle": "x"}), json!({"align": "left"})] {
            s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
            let mut p = json!({"text": PLATE});
            p.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
            assert!(s.execute("sketch.insert_svg", &p).is_err(), "{bad}");
        }
    }

    /// A 20 × 10 rectangle drawn at (10, 10) on a 100 × 50 sheet.
    const ON_SHEET: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="50mm" viewBox="0 0 100 50"><rect x="10" y="10" width="20" height="10"/></svg>"#;

    /// `width` and `height` size the drawn geometry unless `size_from: viewbox` makes them the
    /// sheet's, so a drawing that does not fill its sheet keeps its place and size on it.
    #[test]
    fn an_svg_can_be_sized_by_its_sheet() {
        let near = |a: (f64, f64, f64, f64), e: (f64, f64, f64, f64)| {
            assert!((a.0 - e.0).abs() < 1e-6 && (a.1 - e.1).abs() < 1e-6 && (a.2 - e.2).abs() < 1e-6 && (a.3 - e.3).abs() < 1e-6, "{a:?} vs {e:?}")
        };
        let inserted = |params: serde_json::Value| {
            let mut s = Session::default();
            s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
            let mut p = json!({"text": ON_SHEET});
            p.as_object_mut().unwrap().extend(params.as_object().unwrap().clone());
            s.execute("sketch.insert_svg", &p).unwrap();
            span(&s)
        };
        // By the geometry (as before): the rectangle itself becomes 200 wide, placed at the origin.
        near(inserted(json!({"width": 200})), (100.0, -200.0, 300.0, -100.0));
        // By the sheet: the sheet becomes 200 wide, so the rectangle is 40 × 20 at (20, −40).
        near(inserted(json!({"width": 200, "size_from": "viewbox"})), (20.0, -40.0, 60.0, -20.0));
        near(inserted(json!({"size_from": "viewbox"})), (10.0, -20.0, 30.0, -10.0));
        // Centred on `at` by the sheet's middle (50, −25): the rectangle sits above and left of it.
        near(inserted(json!({"size_from": "viewbox", "align": "center", "at": [50, 50]})), (10.0, 55.0, 30.0, 65.0));
        let mut s = Session::default();
        let b = s.execute("sketch.drawing_bounds", &json!({"text": ON_SHEET})).unwrap();
        assert_eq!((b["width"].as_f64(), b["sheet"]["width"].as_f64(), b["sheet"]["height"].as_f64()), (Some(20.0), Some(100.0), Some(50.0)), "{b}");
        let b = s.execute("sketch.drawing_bounds", &json!({"text": r#"<svg><rect width="20" height="10"/></svg>"#})).unwrap();
        assert!(b.get("sheet").is_none(), "no viewBox, no width: {b}");
        // No sheet to size by, a DXF, and a wrong word are refused.
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let bare = json!({"text": r#"<svg><rect width="20" height="10"/></svg>"#, "size_from": "viewbox", "width": 50});
        assert!(s.execute("sketch.insert_svg", &bare).unwrap_err().to_string().contains("viewBox"));
        let dxf = json!({"text": "0\nSECTION\n2\nENTITIES\n0\nLINE\n8\n0\n10\n0\n20\n0\n11\n10\n21\n0\n0\nENDSEC\n0\nEOF\n", "size_from": "viewbox"});
        assert!(s.execute("sketch.insert_dxf", &dxf).unwrap_err().to_string().contains("DXF"));
        let word = json!({"text": ON_SHEET, "size_from": "sheet"});
        assert!(s.execute("sketch.insert_svg", &word).unwrap_err().to_string().contains("size_from"));
    }

    /// `fixed: true` fixes every point the drawing adds, so the sketch has nothing left to move.
    #[test]
    fn an_inserted_drawing_can_come_in_fixed() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let r = s.execute("sketch.insert_svg", &json!({"text": PLATE, "fixed": true})).unwrap();
        assert_eq!((r["curves"].as_u64(), r["fixed"].as_u64()), (Some(4), Some(4)), "{r}");
        assert_eq!(r["created"]["constraints"].as_array().map(Vec::len), Some(4), "{r}");
        let solved = s.execute("sketch.solve", &json!({})).unwrap();
        assert_eq!(solved["dof"], json!(0), "{solved}");
        // Without it, as before: nothing fixed, four free points.
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let r = s.execute("sketch.insert_svg", &json!({"text": PLATE})).unwrap();
        assert!(r.get("fixed").is_none() && r["created"]["constraints"].as_array().is_some_and(Vec::is_empty), "{r}");
        // A circle: its centre is fixed, its radius stays free.
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let circle = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20"><circle cx="10" cy="10" r="5"/></svg>"#;
        let r = s.execute("sketch.insert_svg", &json!({"text": circle, "fixed": true})).unwrap();
        assert_eq!(r["fixed"], json!(1), "{r}");
        assert_eq!(s.execute("sketch.solve", &json!({})).unwrap()["dof"], json!(1), "the radius");
    }

    /// A NACA 00xx section (the public four-digit formula), `n` points per side on a cosine
    /// spacing, as a Selig file. The formula's trailing edge is open; `closed` uses the -0.1036
    /// last term that brings the sides together at x = 1.
    fn naca_00(thickness: f64, n: usize, closed: bool) -> String {
        let k4 = if closed { 0.1036 } else { 0.1015 };
        let yt = |x: f64| 5.0 * thickness * (0.2969 * x.sqrt() - 0.1260 * x - 0.3516 * x * x + 0.2843 * x.powi(3) - k4 * x.powi(4));
        let xs: Vec<f64> = (0..=n).map(|i| 0.5 * (1.0 - (std::f64::consts::PI * i as f64 / n as f64).cos())).collect();
        let mut text = format!("NACA 00{:02}\n", (thickness * 100.0).round() as usize);
        for &x in xs.iter().rev() {
            text.push_str(&format!("{x:.7} {:.7}\n", yt(x)));
        }
        for &x in xs.iter().skip(1) {
            text.push_str(&format!("{x:.7} {:.7}\n", -yt(x)));
        }
        text
    }

    /// The area of that section at chord `c`, in closed form: 2 ∫ y_t dx.
    fn naca_00_area(thickness: f64, c: f64, closed: bool) -> f64 {
        let k4 = if closed { 0.1036 } else { 0.1015 };
        10.0 * thickness * c * c * (0.2969 * 2.0 / 3.0 - 0.1260 / 2.0 - 0.3516 / 3.0 + 0.2843 / 4.0 - k4 / 5.0)
    }

    fn total_volume(s: &Session) -> f64 {
        s.model.state().bodies.iter().map(|b| solidcraft_kernel::measure(&b.body).unwrap().volume).sum()
    }

    fn profile_areas(s: &Session, id: u64) -> Vec<f64> {
        s.model.state().sketch(id).unwrap().profiles.iter().map(|p| p.area).collect()
    }

    /// #14202: a NACA 0012 at a 100 mm chord comes in as one spline through its 121 points and a
    /// line across the open trailing edge, one profile whose area is the formula's (0.685083 t c²
    /// = 822.1 mm²), and extruded 10 mm it is 8221 mm³. The spline bows between the file's points
    /// (most at the nose), so the area is within a quarter of a percent, not exact.
    #[test]
    fn a_naca_0012_section_extrudes_to_its_area_times_the_depth() {
        let mut s = Session::default();
        let r = s.execute("sketch.insert_airfoil", &json!({"text": naca_00(0.12, 60, false), "chord": 100})).unwrap();
        assert_eq!(r["format"], "selig", "{r}");
        assert_eq!(r["name"], "NACA 0012", "{r}");
        assert_eq!(r["points"], 121, "{r}");
        assert_eq!(r["merged"], 0, "{r}");
        assert_eq!(r["curves"], 2, "{r}");
        assert_eq!(r["trailing_edge"], "open", "{r}");
        assert_eq!(r["created_sketch"], r["sketch"], "{r}");
        // y_t(1) = 5 t · 0.0021 = 0.00126 each side.
        assert!((r["te_gap"].as_f64().unwrap() - 0.252).abs() < 1e-6, "{r}");
        assert!(r["deviation"].as_f64().unwrap() < 1e-9, "{r}");
        let bulge = r["bulge"].as_f64().unwrap();
        assert!(bulge > 0.0 && bulge < 0.05, "{r}");
        let id = r["sketch"].as_u64().unwrap();
        let areas = profile_areas(&s, id);
        let expected = naca_00_area(0.12, 100.0, false);
        assert!((expected - 822.1).abs() < 1e-3, "{expected}");
        assert_eq!(areas.len(), 1, "{areas:?}");
        assert!((areas[0] - expected).abs() < 2.5e-3 * expected, "area {} vs {expected}", areas[0]);
        s.execute("sketch.finish", &json!({})).unwrap();
        s.execute("solid.extrude", &json!({"distance": 10})).unwrap();
        let v = total_volume(&s);
        assert!((v - 8221.0).abs() < 2.5e-3 * 8221.0, "volume {v} vs 8221.0");
    }

    /// #14202: two sections, 100 mm and 60 mm chords 300 mm apart, loft to the volume of the
    /// linear taper: 0.685083 t · 300 · (100² + 100·60 + 60²) / 3 = 161 131.6 mm³.
    #[test]
    fn two_sections_loft_to_the_volume_of_the_linear_taper() {
        let mut s = Session::default();
        for (name, chord, z) in [("Root", 100.0, 0.0), ("Tip", 60.0, 300.0)] {
            let plane = json!({"origin": [0, 0, z], "x_dir": [1, 0, 0], "y_dir": [0, 1, 0]});
            let r =
                s.execute("sketch.insert_airfoil", &json!({"text": naca_00(0.12, 60, false), "chord": chord, "plane": plane, "name": name})).unwrap();
            assert_eq!(r["curves"], 2, "{r}");
            s.execute("sketch.finish", &json!({})).unwrap();
        }
        s.execute("solid.loft", &json!({"sections": [{"sketch": "Root"}, {"sketch": "Tip"}], "body_name": "Wing"})).unwrap();
        let expected = naca_00_area(0.12, 1.0, false) * 300.0 * (100.0f64.powi(2) + 100.0 * 60.0 + 60.0f64.powi(2)) / 3.0;
        assert!((expected - 161_131.6).abs() < 0.1, "{expected}");
        let v = total_volume(&s);
        assert!((v - expected).abs() < 3e-3 * expected, "volume {v} vs {expected}");
    }

    /// #14202: a Lednicer file as the databases write it (CRLF, a comment, counts as `61.0` and
    /// `61.`, the nose on both sides) with a closed trailing edge comes in as one spline that ends
    /// where it starts, placed by its quarter-chord point, turned and mirrored.
    #[test]
    fn a_lednicer_section_is_placed_by_its_pivot_turned_and_flipped() {
        let selig = naca_00(0.12, 60, true);
        let pts: Vec<(f64, f64)> =
            selig.lines().skip(1).map(|l| l.split_once(' ').map(|(x, y)| (x.parse().unwrap(), y.parse().unwrap())).unwrap()).collect();
        assert_eq!(pts.len(), 121);
        let mut text = String::from("# a comment\r\nNACA 0012 lednicer\r\n      61.0      61.\r\n   \r\n");
        for (x, y) in pts[..=60].iter().rev() {
            text.push_str(&format!("  {x:.7}  {y:.7}\r\n"));
        }
        text.push_str("\r\n");
        for (x, y) in &pts[60..] {
            text.push_str(&format!("  {x:.7}  {y:.7}\r\n"));
        }
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        let r = s
            .execute("sketch.insert_airfoil", &json!({"text": text, "chord": 100, "pivot": 0.25, "at": [10, 5], "angle": 90, "flip": true}))
            .unwrap();
        assert_eq!(r["format"], "lednicer", "{r}");
        assert_eq!(r["name"], "NACA 0012 lednicer", "{r}");
        assert_eq!(r["points"], 121, "{r}");
        assert_eq!(r["trailing_edge"], "closed", "{r}");
        assert_eq!(r["te_gap"], 0.0, "{r}");
        assert_eq!(r["curves"], 1, "{r}");
        assert_eq!(r["created_sketch"], serde_json::Value::Null, "{r}");
        let id = r["sketch"].as_u64().unwrap();
        let areas = profile_areas(&s, id);
        let expected = naca_00_area(0.12, 100.0, true);
        assert_eq!(areas.len(), 1, "{areas:?}");
        assert!((areas[0] - expected).abs() < 2.5e-3 * expected, "area {} vs {expected}", areas[0]);
        // The quarter-chord point sits at `at`; turned 90°, the chord runs up the sketch's y: the
        // nose 25 mm below `at`, the trailing edge 75 mm above it. Mirrored, the upper side is on
        // the +x side. The sketch's points are the file's, so the nose and tail are exact.
        let st = s.model.state();
        let sk = &st.sketch(id).unwrap().sketch;
        let has = |x: f64, y: f64| sk.points.iter().any(|p| (p.pos.x - x).abs() < 1e-9 && (p.pos.y - y).abs() < 1e-9);
        assert!(has(10.0, -20.0) && has(10.0, 80.0), "{:?}", sk.points);
        let (lo, hi) = (span(&s).0, span(&s).2);
        assert!((lo - 4.0).abs() < 0.02 && (hi - 16.0).abs() < 0.02, "{lo} {hi}");
        // The thickest point of the upper side (0.06 c at 0.3 c; the nearest file point is at
        // 0.2966 c) lands 6 mm to the +x side of `at`, 5 mm up the chord from it.
        let widest = sk.points.iter().map(|p| p.pos).max_by(|a, b| a.x.total_cmp(&b.x)).unwrap();
        assert!((widest.x - 16.0).abs() < 0.005 && (widest.y - 9.66).abs() < 0.05, "{widest:?}");
    }

    /// #14202: errors name the file and the line, a placeholder is never read as 0, more points
    /// than a spline takes is said with both numbers, and a refused insert leaves no sketch.
    #[test]
    fn airfoil_errors_name_the_file_and_the_line() {
        let dir = std::env::temp_dir().join(format!("solidcraft-14202-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.dat");
        std::fs::write(&junk, "NACA 23015\n  ......\n0.5 0.1\n").unwrap();
        let mut s = Session::default();
        let e = s.execute("sketch.insert_airfoil", &json!({"path": junk.to_string_lossy(), "chord": 100})).unwrap_err().to_string();
        assert_eq!(e, "junk.dat: line 2: expected two numbers, found 1: `......`");
        let e = s.execute("sketch.insert_airfoil", &json!({"text": "Name\n1 0\n0.5 ......\n", "chord": 100})).unwrap_err().to_string();
        assert_eq!(e, "airfoil: line 3: not a number: `......`");
        let e = s.execute("sketch.insert_airfoil", &json!({"text": naca_00(0.12, 300, false), "chord": 100})).unwrap_err().to_string();
        assert_eq!(e, "airfoil: 601 points; a spline takes at most 500");
        let good = naca_00(0.12, 20, false);
        for (p, part) in [
            (json!({"text": good, "chord": 0}), "`chord` must be a positive number (mm)"),
            (json!({"text": good}), "`chord` must be a positive number (mm)"),
            (json!({"text": good, "chord": 100, "pivot": "x"}), "`pivot` must be a number"),
            (json!({"text": good, "chord": 100, "angle": "x"}), "`angle` must be a number (degrees)"),
            (json!({"text": good, "chord": 100, "flip": "yes"}), "`flip` must be true or false"),
            (json!({"text": good, "chord": 100, "format": "csv"}), "`format` must be \"selig\" or \"lednicer\""),
            (json!({"chord": 100}), "give `path` or `text`"),
            (json!({"text": "hello", "chord": 100}), "airfoil: no coordinates"),
            (json!({"text": good, "chord": 10, "tolerance": 1000}), "points left within a tolerance of 10 mm; an airfoil needs at least four"),
            (json!({"path": dir.join("missing.dat").to_string_lossy(), "chord": 100}), "missing.dat"),
        ] {
            let e = s.execute("sketch.insert_airfoil", &p).unwrap_err().to_string();
            assert!(e.contains(part), "{p}: {e}");
        }
        assert!(s.active_sketch.is_none(), "a refused insert leaves no empty sketch behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #14202: two points of the file within the tolerance become one sketch point, counted and
    /// measured; a trailing edge within it is closed; with a tighter tolerance both stay apart.
    #[test]
    fn points_within_the_tolerance_merge_and_the_reply_measures_it() {
        // The upper side has a point 7e-7 chords (7e-5 mm at 100 mm) after another; the trailing
        // edge is 0.002 chords (0.2 mm) open.
        let text = "Tiny\n1 0.001\n0.5 0.05\n0.5000005 0.0500005\n0 0\n0.5 -0.05\n1 -0.001\n";
        let mut s = Session::default();
        let r = s.execute("sketch.insert_airfoil", &json!({"text": text, "chord": 100})).unwrap();
        assert_eq!((r["points"].as_u64(), r["merged"].as_u64(), r["curves"].as_u64()), (Some(6), Some(1), Some(2)), "{r}");
        assert_eq!(r["trailing_edge"], "open", "{r}");
        assert!((r["te_gap"].as_f64().unwrap() - 0.2).abs() < 1e-9, "{r}");
        let d = r["deviation"].as_f64().unwrap();
        assert!(d > 0.0 && d <= 1e-4, "{r}");
        s.execute("sketch.finish", &json!({})).unwrap();
        let r = s.execute("sketch.insert_airfoil", &json!({"text": text, "chord": 100, "tolerance": 1e-6})).unwrap();
        assert_eq!((r["merged"].as_u64(), r["deviation"].as_f64()), (Some(0), Some(0.0)), "{r}");
        s.execute("sketch.finish", &json!({})).unwrap();
        let r = s.execute("sketch.insert_airfoil", &json!({"text": text, "chord": 100, "tolerance": 0.5})).unwrap();
        assert_eq!((r["trailing_edge"].as_str(), r["curves"].as_u64()), (Some("closed"), Some(1)), "{r}");
        assert!(r["deviation"].as_f64().unwrap() > 0.05, "the last point is 0.2 mm from where the spline ends: {r}");
        assert_eq!(profile_areas(&s, r["sketch"].as_u64().unwrap()).len(), 1);
    }

    #[test]
    fn export_dxf_default_is_byte_identical_to_before() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        s.execute("sketch.rectangle.two_point", &json!({"p0": [0, 0], "p1": [40, 30]})).unwrap();
        s.execute("sketch.circle.center", &json!({"center": [15, 15], "radius": 5})).unwrap();

        let out = s.execute("sketch.export_dxf", &json!({})).unwrap();
        let text = out["dxf"].as_str().unwrap();

        // Build expected R12 DXF exactly as before
        let st = s.model.state();
        let ss = st.sketch(1).unwrap();
        let sk = &ss.sketch;
        let mut geom = Vec::new();
        for (i, c) in sk.curves.iter().enumerate() {
            if c.construction {
                continue;
            }
            let pt = |k: usize| sk.point(k).unwrap_or_default();
            match &c.kind {
                solidcraft_sketch::CurveKind::Line { a, b } => geom.push(Geom2::Line(pt(*a), pt(*b))),
                solidcraft_sketch::CurveKind::Circle { c, r } => geom.push(Geom2::Circle(pt(*c), *r)),
                solidcraft_sketch::CurveKind::Arc { c, a, b } => geom.push(Geom2::Arc { c: pt(*c), a: pt(*a), b: pt(*b) }),
                _ => {
                    let poly = sk.polyline(i);
                    geom.extend(poly.windows(2).map(|w| Geom2::Line(w[0], w[1])));
                }
            }
        }
        let expected = solidcraft_io::write_dxf(&geom);
        assert_eq!(text, expected, "default output must be byte-identical to previous write_dxf output");

        // Null options must be treated as omitted (default behavior)
        for null_params in [
            json!({"units": null}),
            json!({"layer": null}),
            json!({"layers": null}),
            json!({"join": null}),
            json!({"path": null}),
            json!({"units": null, "layer": null, "layers": null, "join": null, "path": null}),
        ] {
            let out_null = s.execute("sketch.export_dxf", &null_params).unwrap();
            assert_eq!(out_null["dxf"].as_str().unwrap(), expected, "null options must behave identically to default options");
            assert_eq!(out_null["entities"], out["entities"]);
        }

        // Hostile inputs
        assert!(s.execute("sketch.export_dxf", &json!({"units": 123})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"units": "unknown_unit"})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"layer": 456})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"layers": 123})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"layers": "all"})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"join": "true"})).is_err());
        assert!(s.execute("sketch.export_dxf", &json!({"path": 123})).is_err());
    }

    #[test]
    fn export_dxf_layers_present() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        s.execute("sketch.rectangle.two_point", &json!({"p0": [0, 0], "p1": [50, 50]})).unwrap();
        s.execute("sketch.circle.center", &json!({"center": [25, 25], "radius": 10})).unwrap();

        // 1. Single custom layer
        let out_cut = s.execute("sketch.export_dxf", &json!({"layer": "CUT"})).unwrap();
        let dxf_cut = out_cut["dxf"].as_str().unwrap();
        assert!(dxf_cut.contains("\n8\nCUT\n"));
        assert!(!dxf_cut.contains("\n8\n0\n"));

        // 2. Roles layers (OUTLINE for outer profile, CUT for holes)
        let out_roles = s.execute("sketch.export_dxf", &json!({"layers": "roles"})).unwrap();
        let dxf_roles = out_roles["dxf"].as_str().unwrap();
        assert!(dxf_roles.contains("\n8\nOUTLINE\n"));
        assert!(dxf_roles.contains("\n8\nCUT\n"));

        // 3. Roles layers with join: true
        let out_joined_roles = s.execute("sketch.export_dxf", &json!({"join": true, "layers": "roles"})).unwrap();
        assert_eq!(out_joined_roles["entities"], 2);
        let dxf_joined = out_joined_roles["dxf"].as_str().unwrap();
        assert!(dxf_joined.contains("AcDbPolyline"));
        assert!(dxf_joined.contains("\n  8\nOUTLINE\n") || dxf_joined.contains("\n8\nOUTLINE\n"));
        assert!(dxf_joined.contains("\n  8\nCUT\n") || dxf_joined.contains("\n8\nCUT\n"));
    }

    #[test]
    fn export_dxf_join_does_not_duplicate_holes() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        s.execute("sketch.rectangle.two_point", &json!({"p0": [0, 0], "p1": [100, 80]})).unwrap();
        s.execute("sketch.circle.center", &json!({"center": [30, 40], "radius": 5})).unwrap();
        s.execute("sketch.circle.center", &json!({"center": [70, 40], "radius": 8})).unwrap();

        // With join: true, layers: "roles": exactly 3 entities (1 outer outline, 2 hole cuts)
        let out_roles = s.execute("sketch.export_dxf", &json!({"join": true, "layers": "roles"})).unwrap();
        assert_eq!(out_roles["entities"], 3, "expected 1 outer outline and 2 hole cuts, got {}", out_roles["entities"]);
        let dxf_roles = out_roles["dxf"].as_str().unwrap();
        let lwpolyline_count = dxf_roles.lines().filter(|l| l.trim() == "LWPOLYLINE").count();
        assert_eq!(lwpolyline_count, 3, "expected exactly 3 LWPOLYLINE entities in DXF");

        // Without roles (single layer): still exactly 3 entities
        let out_single = s.execute("sketch.export_dxf", &json!({"join": true})).unwrap();
        assert_eq!(out_single["entities"], 3, "entities without roles must also be exactly 3");
    }

    #[test]
    fn export_dxf_join_closed_lwpolyline_with_bulge() {
        let mut s = Session::default();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        // Quarter-pie shape: center at (0, 0), arc from (10, 0) to (0, 10), and lines to (0, 0)
        s.execute("sketch.line", &json!({"points": [[0, 0], [10, 0]]})).unwrap();
        s.execute("sketch.arc.center_point", &json!({"center": [0, 0], "start": [10, 0], "end": [0, 10]})).unwrap();
        s.execute("sketch.line", &json!({"points": [[0, 10], [0, 0]]})).unwrap();

        let out = s.execute("sketch.export_dxf", &json!({"join": true})).unwrap();
        let dxf = out["dxf"].as_str().unwrap();
        assert!(dxf.contains("LWPOLYLINE"));
        // 70: bit 1 (closed) + bit 128 (plinegen) = 129
        assert!(dxf.contains("\n 70\n129\n") || dxf.contains("\n70\n129\n"));
        // Bulge: 90° arc has sweep = pi/2, bulge = tan(sweep / 4) = tan(pi / 8) = sqrt(2) - 1 ≈ 0.414213562373
        let expected_bulge = (std::f64::consts::FRAC_PI_2 / 4.0).tan();
        let has_bulge = dxf
            .lines()
            .zip(dxf.lines().skip(1))
            .any(|(k, v)| k.trim() == "42" && (v.trim().parse::<f64>().unwrap_or(0.0).abs() - expected_bulge).abs() < 1e-6);
        assert!(has_bulge, "DXF must contain group 42 bulge approx {expected_bulge}");

        // Re-import the joined DXF into a new sketch and verify round-trip profile area
        s.execute("sketch.finish", &json!({})).unwrap();
        let r = s.execute("sketch.insert_dxf", &json!({"text": dxf, "at": [100, 0]})).unwrap();
        let id = r["sketch"].as_u64().unwrap();
        let ss = s.model.state().sketch(id).unwrap().clone();
        assert_eq!(ss.profiles.len(), 1);
        let expected_area = std::f64::consts::PI * 100.0 / 4.0; // quarter circle of radius 10
        assert!((ss.profiles[0].area - expected_area).abs() < 1e-4, "area was {}", ss.profiles[0].area);
    }

    #[test]
    fn export_dxf_inch_design_round_trip() {
        let mut s = Session::default();
        s.doc_mut().units = "in".to_string();
        s.execute("sketch.create", &json!({"plane": "XY"})).unwrap();
        // A 2 in by 3 in rectangle in an inch design (stored internally as 50.8 mm by 76.2 mm)
        s.execute("sketch.rectangle.two_point", &json!({"p0": [0, 0], "p1": [50.8, 76.2]})).unwrap();

        // 1. Export with units: "design"
        let out = s.execute("sketch.export_dxf", &json!({"units": "design"})).unwrap();
        let dxf = out["dxf"].as_str().unwrap();
        // $INSUNITS must be 1 (inches)
        assert!(dxf.contains("\n$INSUNITS\n70\n1\n"));
        // Sizes in inches: 2.0 and 3.0
        assert!(dxf.contains("2.000000000"));
        assert!(dxf.contains("3.000000000"));

        // Round-trip through read_dxf: reads $INSUNITS=1, scales by 25.4 to mm
        let geom = solidcraft_io::read_dxf(dxf).unwrap();
        let mut widths = Vec::new();
        let mut heights = Vec::new();
        for g in &geom {
            if let Geom2::Line(a, b) = g {
                let dx = (b.x - a.x).abs() / 25.4;
                let dy = (b.y - a.y).abs() / 25.4;
                if dx > 0.1 {
                    widths.push(dx);
                }
                if dy > 0.1 {
                    heights.push(dy);
                }
            }
        }
        for w in widths {
            assert!((w - 2.0).abs() < 1e-8, "width in inches was {w}");
        }
        for h in heights {
            assert!((h - 3.0).abs() < 1e-8, "height in inches was {h}");
        }

        // 2. Export with join: true and explicit units: "in"
        let out_join = s.execute("sketch.export_dxf", &json!({"join": true, "units": "in"})).unwrap();
        let dxf_join = out_join["dxf"].as_str().unwrap();
        assert!(dxf_join.contains("LWPOLYLINE"));
        assert!(dxf_join.contains("\n$INSUNITS\n 70\n1\n") || dxf_join.contains("\n$INSUNITS\n70\n1\n"));

        let geom_join = solidcraft_io::read_dxf(dxf_join).unwrap();
        assert_eq!(geom_join.len(), 4);
        for g in &geom_join {
            if let Geom2::Line(a, b) = g {
                let len_in = a.dist(*b) / 25.4;
                assert!((len_in - 2.0).abs() < 1e-8 || (len_in - 3.0).abs() < 1e-8, "edge length {len_in}");
            }
        }
    }
}
