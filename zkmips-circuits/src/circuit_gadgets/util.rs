//! Utility traits, functions used in the crate.
use crate::mips_types::{Field};
use halo2_proofs::plonk::Expression;
use crate::circuit_gadgets::Expr;

/// Returns the sum of the passed in cells
pub mod sum {
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;
    use crate::circuit_gadgets::Expr;

    /// Returns an expression for the sum of the list of expressions.
    pub fn expr<F: Field, E: Expr<F>, I: IntoIterator<Item = E>>(inputs: I) -> Expression<F> {
        inputs
            .into_iter()
            .fold(0.expr(), |acc, input| acc + input.expr())
    }

    /// Returns the sum of the given list of values within the field.
    pub fn value<F: Field>(values: &[u8]) -> F {
        values
            .iter()
            .fold(F::ZERO, |acc, value| acc + F::from(*value as u64))
    }
}

/// Returns `1` when `expr[0] && expr[1] && ... == 1`, and returns `0`
/// otherwise. Inputs need to be boolean
pub mod and {
    use crate::circuit_gadgets::Expr;
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;

    /// Returns an expression that evaluates to 1 only if all the expressions in
    /// the given list are 1, else returns 0.
    pub fn expr<F: Field, E: Expr<F>, I: IntoIterator<Item = E>>(inputs: I) -> Expression<F> {
        inputs
            .into_iter()
            .fold(1.expr(), |acc, input| acc * input.expr())
    }

    /// Returns the product of all given values.
    pub fn value<F: Field>(inputs: Vec<F>) -> F {
        inputs.iter().fold(F::ONE, |acc, input| acc * input)
    }
}

/// Returns `1` when `expr[0] || expr[1] || ... == 1`, and returns `0`
/// otherwise. Inputs need to be boolean
pub mod or {
    use super::{and, not};
    use crate::circuit_gadgets::Expr;
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;

    /// Returns an expression that evaluates to 1 if any expression in the given
    /// list is 1. Returns 0 if all the expressions were 0.
    pub fn expr<F: Field, E: Expr<F>, I: IntoIterator<Item = E>>(inputs: I) -> Expression<F> {
        not::expr(and::expr(inputs.into_iter().map(not::expr)))
    }

    /// Returns the value after passing all given values through the OR gate.
    pub fn value<F: Field>(inputs: Vec<F>) -> F {
        not::value(and::value(inputs.into_iter().map(not::value).collect()))
    }
}

/// Returns `1` when `b == 0`, and returns `0` otherwise.
/// `b` needs to be boolean
pub mod not {
    use crate::circuit_gadgets::Expr;
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;

    /// Returns an expression that represents the NOT of the given expression.
    pub fn expr<F: Field, E: Expr<F>>(b: E) -> Expression<F> {
        1.expr() - b.expr()
    }

    /// Returns a value that represents the NOT of the given value.
    pub fn value<F: Field>(b: F) -> F {
        F::ONE - b
    }
}

/// Returns `a ^ b`.
/// `a` and `b` needs to be boolean
pub mod xor {
    use crate::circuit_gadgets::Expr;
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;

    /// Returns an expression that represents the XOR of the given expression.
    pub fn expr<F: Field, E: Expr<F>>(a: E, b: E) -> Expression<F> {
        a.expr() + b.expr() - 2.expr() * a.expr() * b.expr()
    }

    /// Returns a value that represents the XOR of the given value.
    pub fn value<F: Field>(a: F, b: F) -> F {
        a + b - F::from(2u64) * a * b
    }
}

/// Returns `when_true` when `selector == 1`, and returns `when_false` when
/// `selector == 0`. `selector` needs to be boolean.
pub mod select {
    use crate::circuit_gadgets::Expr;
    use crate::mips_types::Field;
    use halo2_proofs::plonk::Expression;

    /// Returns the `when_true` expression when the selector is true, else
    /// returns the `when_false` expression.
    pub fn expr<F: Field>(
        selector: Expression<F>,
        when_true: Expression<F>,
        when_false: Expression<F>,
    ) -> Expression<F> {
        selector.clone() * when_true + (1.expr() - selector) * when_false
    }

    /// Returns the `when_true` value when the selector is true, else returns
    /// the `when_false` value.
    pub fn value<F: Field>(selector: F, when_true: F, when_false: F) -> F {
        selector * when_true + (F::ONE - selector) * when_false
    }

    /// Returns the `when_true` word when selector is true, else returns the
    /// `when_false` word.
    pub fn value_word<F: Field>(
        selector: F,
        when_true: [u8; 32],
        when_false: [u8; 32],
    ) -> [u8; 32] {
        if selector == F::ONE {
            when_true
        } else {
            when_false
        }
    }
}


impl<F: Field> Expr<F> for Expression<F> {
    #[inline]
    fn expr(&self) -> Expression<F> {
        self.clone()
    }
}

impl<F: Field> Expr<F> for &Expression<F> {
    #[inline]
    fn expr(&self) -> Expression<F> {
        (*self).clone()
    }
}

impl<F: Field> Expr<F> for i32 {
    #[inline]
    fn expr(&self) -> Expression<F> {
        Expression::Constant(
            F::from(self.unsigned_abs() as u64) * if self.is_negative() { -F::ONE } else { F::ONE },
        )
    }
}

/// Given a bytes-representation of an expression, it computes and returns the
/// single expression.
pub fn expr_from_bytes<F: Field, E: Expr<F>>(bytes: &[E]) -> Expression<F> {
    let mut value = 0.expr();
    let mut multiplier = F::ONE;
    for byte in bytes.iter() {
        value = value + byte.expr() * multiplier;
        multiplier *= F::from(256);
    }
    value
}

/// Returns 2**by as Field
pub fn pow_of_two<F: Field>(by: usize) -> F {
    F::from(2).pow([by as u64, 0, 0, 0])
}
