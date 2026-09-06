//! Scalar maths that procedural modelling needs and Steel's prelude does not carry.
//!
//! Steel already provides `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `exp`, `log`,
//! `sqrt`, `expt`, `square`, `abs`, `floor`, `ceiling`, `round`, `truncate`, `min`,
//! `max`, `modulo`, `remainder`, `quotient`, `gcd`, `lcm`, and the numeric predicates.
//! What it lacks is exactly what placing geometry keeps asking for: pi, the two-argument
//! arctangent, degree conversion, and the shaping helpers.
//!
//! Every name here becomes a module-required identifier, which Steel refuses to `set!`.
//! They are therefore chosen to be ones a caller is unlikely to want to rebind -- `e`
//! is deliberately absent for that reason, since `(lambda (e) ...)` is the usual
//! exception-handler idiom and `(exp 1)` covers the need.

use std::{
    f64::consts::{PI, TAU},
    sync::Mutex,
};

use steel::{
    SteelVal,
    primitives::numbers::{realp, steel_inexact},
    rerrs::{ErrorKind, SteelErr},
    rvals::{FromSteelVal, IntoSteelVal, Result as SteelResult},
    steel_vm::{engine::Engine, register_fn::RegisterFn},
};

/// Steel's `f64` binding accepts only inexact numbers. Modelling inputs use the
/// whole real numeric tower, with Steel responsible for rounding exact values.
struct Real(f64);

impl FromSteelVal for Real {
    fn from_steelval(value: &SteelVal) -> SteelResult<Self> {
        if !realp(value) {
            return Err(SteelErr::new(
                ErrorKind::TypeMismatch,
                format!("expected a real number, found: {value}"),
            ));
        }
        if let SteelVal::NumV(number) = value {
            return Ok(Self(*number));
        }
        let inexact = steel_inexact(std::slice::from_ref(value))?;
        f64::from_steelval(&inexact).map(Self)
    }
}

/// Seeded so a scene built from random placement is reproducible. A run that wants
/// fresh values calls `(random-seed! n)` explicitly.
const DEFAULT_SEED: u64 = 0x2545_F491_4F6C_DD1D;

struct Rng(Mutex<u64>);

impl Rng {
    /// xorshift64*, which is small, has no dependencies, and is far better than
    /// anything a caller would hand-roll in Scheme.
    fn next_f64(&self) -> f64 {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut value = *state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        *state = value;
        let scrambled = value.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // The top 32 bits, which `u32` holds exactly and `f64` represents exactly, so
        // this needs no lossy cast. The shift guarantees the conversion cannot fail.
        let bits = u32::try_from(scrambled >> 32).unwrap_or(u32::MAX);
        f64::from(bits) / (f64::from(u32::MAX) + 1.0)
    }

    fn reseed(&self, seed: i64) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A zero state is a fixed point for xorshift, so fold it away.
        *state = match seed.unsigned_abs() {
            0 => DEFAULT_SEED,
            seed => seed,
        };
    }
}

/// `t` clamped to `[0, 1]`, the convention every shaping helper below shares.
fn unit(t: f64) -> f64 {
    t.clamp(0.0, 1.0)
}

pub(super) fn register_math_functions(engine: &mut Engine) {
    engine.register_value("pi", SteelVal::NumV(PI));
    engine.register_value("tau", SteelVal::NumV(TAU));

    // ---- trigonometry Steel omits ----
    // The two-argument arctangent: the angle of (x, y), quadrant-correct. Steel's
    // `atan` takes one argument, so this is not reachable without it.
    engine.register_fn("atan2", |Real(y): Real, Real(x): Real| y.atan2(x));
    engine.register_fn("sinh", |Real(value): Real| value.sinh());
    engine.register_fn("cosh", |Real(value): Real| value.cosh());
    engine.register_fn("tanh", |Real(value): Real| value.tanh());
    engine.register_fn("asinh", |Real(value): Real| value.asinh());
    engine.register_fn("acosh", |Real(value): Real| value.acosh());
    engine.register_fn("atanh", |Real(value): Real| value.atanh());
    engine.register_fn("hypot", |Real(x): Real, Real(y): Real| x.hypot(y));

    // ---- logarithms and roots ----
    engine.register_fn("log2", |Real(value): Real| value.log2());
    engine.register_fn("log10", |Real(value): Real| value.log10());
    engine.register_fn("cbrt", |Real(value): Real| value.cbrt());

    // ---- angles ----
    // Blender's rotation properties are radians while everything a human states is in
    // degrees, so this conversion is written constantly.
    engine.register_fn("degrees->radians", |Real(value): Real| value.to_radians());
    engine.register_fn("radians->degrees", |Real(value): Real| value.to_degrees());

    // ---- shaping ----
    engine.register_fn(
        "clamp",
        |Real(value): Real, Real(low): Real, Real(high): Real| -> f64 {
            // Tolerate a reversed range rather than returning NaN, which `f64::clamp`
            // would panic on.
            value.clamp(low.min(high), low.max(high))
        },
    );
    engine.register_fn(
        "lerp",
        |Real(from): Real, Real(to): Real, Real(t): Real| -> f64 {
            // The fused form, so `(lerp a b 1.0)` is exactly `b`.
            t.mul_add(to - from, from)
        },
    );
    engine.register_fn(
        "smoothstep",
        |Real(edge0): Real, Real(edge1): Real, Real(value): Real| -> f64 {
            if (edge1 - edge0).abs() < f64::EPSILON {
                return f64::from(u8::from(value >= edge1));
            }
            let t = unit((value - edge0) / (edge1 - edge0));
            t * t * 2.0_f64.mul_add(-t, 3.0)
        },
    );
    engine.register_fn(
        "remap",
        |Real(value): Real,
         Real(from_low): Real,
         Real(from_high): Real,
         Real(to_low): Real,
         Real(to_high): Real|
         -> f64 {
            if (from_high - from_low).abs() < f64::EPSILON {
                return to_low;
            }
            let t = (value - from_low) / (from_high - from_low);
            t.mul_add(to_high - to_low, to_low)
        },
    );
    engine.register_fn("sign", |Real(value): Real| -> f64 {
        if value == 0.0 || value.is_nan() {
            0.0
        } else {
            value.signum()
        }
    });

    // ---- reproducible randomness ----
    let rng = std::sync::Arc::new(Rng(Mutex::new(DEFAULT_SEED)));
    let generator = std::sync::Arc::clone(&rng);
    engine.register_fn("random", move || -> f64 { generator.next_f64() });
    let generator = std::sync::Arc::clone(&rng);
    engine.register_fn(
        "random-range",
        move |Real(low): Real, Real(high): Real| -> f64 {
            low + generator.next_f64() * (high - low)
        },
    );
    let generator = std::sync::Arc::clone(&rng);
    engine.register_fn("random-seed!", move |seed: i64| -> SteelResult<SteelVal> {
        generator.reseed(seed);
        SteelVal::Void.into_steelval()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(source: &str) -> f64 {
        let mut engine = Engine::new_sandboxed();
        register_math_functions(&mut engine);
        let values = engine
            .compile_and_run_raw_program(source.to_owned())
            .expect("runs");
        match values.last().expect("a value") {
            SteelVal::NumV(number) => *number,
            SteelVal::IntV(integer) => f64::from(i32::try_from(*integer).expect("small integer")),
            other => panic!("expected a number, got {other:?}"),
        }
    }

    #[test]
    fn atan2_is_quadrant_correct() {
        // The whole point of the two-argument form: `atan` alone cannot tell these apart.
        assert!((evaluate("(atan2 1.0 1.0)") - PI / 4.0).abs() < 1e-12);
        assert!((evaluate("(atan2 1.0 -1.0)") - 3.0 * PI / 4.0).abs() < 1e-12);
        assert!((evaluate("(atan2 -1.0 -1.0)") + 3.0 * PI / 4.0).abs() < 1e-12);
    }

    #[test]
    fn degrees_round_trip_through_radians() {
        assert!((evaluate("(radians->degrees (degrees->radians 90.0))") - 90.0).abs() < 1e-12);
        assert!((evaluate("(degrees->radians 180.0)") - PI).abs() < 1e-12);
    }

    #[test]
    fn angles_accept_integer_and_exact_fraction_inputs() {
        assert!(evaluate("(degrees->radians 0)").abs() < f64::EPSILON);
        assert!((evaluate("(degrees->radians 180)") - PI).abs() < 1e-12);
        assert!((evaluate("(radians->degrees (degrees->radians 45/2))") - 22.5).abs() < 1e-12);
        assert!((evaluate("(atan2 1 -1.0)") - 3.0 * PI / 4.0).abs() < 1e-12);
        assert!((evaluate("(atan2 -1/2 -1/2)") + 3.0 * PI / 4.0).abs() < 1e-12);
    }

    #[test]
    fn scalar_helpers_accept_mixed_real_numbers() {
        for (source, expected) in [
            ("(hypot 3 4.0)", 5.0),
            ("(sinh 0)", 0.0),
            ("(cosh 0)", 1.0),
            ("(tanh 0)", 0.0),
            ("(asinh 0)", 0.0),
            ("(acosh 1)", 0.0),
            ("(atanh 0)", 0.0),
            ("(log2 8)", 3.0),
            ("(log10 100)", 2.0),
            ("(cbrt -8)", -2.0),
            ("(clamp 20 10.0 0)", 10.0),
            ("(lerp 2 8.0 1/2)", 5.0),
            ("(smoothstep 0 1.0 1/2)", 0.5),
            ("(remap 5 0.0 10 0 100)", 50.0),
            ("(sign -4)", -1.0),
            ("(random-range 7 7.0)", 7.0),
            // Exercise both arbitrary-precision integers and exact fractions.
            ("(sign 9223372036854775808)", 1.0),
            ("(lerp 0 2 9223372036854775808/18446744073709551617)", 1.0),
        ] {
            assert!((evaluate(source) - expected).abs() < 1e-12, "{source}");
        }
    }

    #[test]
    fn real_helpers_reject_non_numbers_and_seeds_remain_integer_only() {
        let mut engine = Engine::new_sandboxed();
        register_math_functions(&mut engine);
        for source in [
            "(degrees->radians #f)",
            "(atan2 1 \"2\")",
            "(hypot 'x 4)",
            "(lerp 0 1 '(1/2))",
            "(clamp 1 0 1+2i)",
            "(random-range 0 #t)",
        ] {
            let error = engine
                .compile_and_run_raw_program(source.to_owned())
                .expect_err("rejects non-real input");
            assert!(
                error.to_string().contains("expected a real number"),
                "{source}: {error}"
            );
        }
        for source in ["(random-seed! 42.0)", "(random-seed! 1/2)"] {
            assert!(
                engine
                    .compile_and_run_raw_program(source.to_owned())
                    .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn lerp_hits_both_endpoints_exactly() {
        assert!((evaluate("(lerp 2.0 8.0 0.0)") - 2.0).abs() < f64::EPSILON);
        assert!((evaluate("(lerp 2.0 8.0 1.0)") - 8.0).abs() < f64::EPSILON);
        assert!((evaluate("(lerp 2.0 8.0 0.5)") - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_tolerates_a_reversed_range() {
        // `f64::clamp` panics when low > high; returning the same answer is friendlier
        // than aborting an evaluation halfway through a scene.
        assert!((evaluate("(clamp 5.0 10.0 0.0)") - 5.0).abs() < f64::EPSILON);
        assert!((evaluate("(clamp 20.0 10.0 0.0)") - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn smoothstep_is_flat_outside_its_edges() {
        assert!((evaluate("(smoothstep 0.0 1.0 -5.0)")).abs() < f64::EPSILON);
        assert!((evaluate("(smoothstep 0.0 1.0 5.0)") - 1.0).abs() < f64::EPSILON);
        assert!((evaluate("(smoothstep 0.0 1.0 0.5)") - 0.5).abs() < 1e-12);
        // A degenerate edge pair must not divide by zero.
        assert!((evaluate("(smoothstep 1.0 1.0 2.0)") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn remap_survives_a_degenerate_source_range() {
        assert!((evaluate("(remap 5.0 0.0 10.0 0.0 100.0)") - 50.0).abs() < 1e-12);
        assert!((evaluate("(remap 5.0 3.0 3.0 7.0 9.0)") - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn random_is_reproducible_from_a_seed_and_stays_in_range() {
        let first = evaluate("(random-seed! 42) (random)");
        let again = evaluate("(random-seed! 42) (random)");
        assert!((first - again).abs() < f64::EPSILON, "{first} != {again}");
        assert!((0.0..1.0).contains(&first), "{first} outside [0, 1)");
        // A zero seed is a fixed point for xorshift and must not lock the generator.
        let zeroed = evaluate("(random-seed! 0) (random) (random)");
        assert!((0.0..1.0).contains(&zeroed));
        assert!(zeroed != 0.0);
    }

    #[test]
    fn sign_reports_zero_for_zero_rather_than_one() {
        assert!((evaluate("(sign -4.0)") + 1.0).abs() < f64::EPSILON);
        assert!((evaluate("(sign 4.0)") - 1.0).abs() < f64::EPSILON);
        assert!((evaluate("(sign 0.0)")).abs() < f64::EPSILON);
    }
}
