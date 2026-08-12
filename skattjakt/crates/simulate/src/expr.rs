//! The calculation model.
//!
//! Section 5 asks for a general layer rather than one hard-coded scenario, so
//! an output is an expression over the inputs — `revenue - costs`,
//! `(return - investment) / investment` — written by whoever defines the
//! simulation and parsed here.
//!
//! Two decisions worth stating, because both were the alternative to something
//! easier and worse.
//!
//! **A parser rather than an embedded scripting language.** A general
//! interpreter would be less code to write and would let a stored expression
//! read files, loop forever, or allocate without bound — in a worker process
//! holding a database connection, on input a customer controls. This language
//! has no loops, no assignment, no I/O and no way to name anything the model
//! did not declare. It terminates because it cannot do otherwise.
//!
//! **Names resolve to slots at compile time.** The alternative is a hash lookup
//! per variable per iteration; at a million iterations and twelve variables
//! that is twelve million hash lookups in the inner loop. Compiling
//! `customers` to "slot 3" turns each one into an array index.

use std::collections::HashMap;

/// Why an expression could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExprError {
    #[error("unexpected character {found:?} at position {position}")]
    UnexpectedCharacter { found: char, position: usize },
    #[error("unexpected end of expression; {expected} was expected")]
    UnexpectedEnd { expected: &'static str },
    #[error("expected {expected}, found {found:?}")]
    Expected {
        expected: &'static str,
        found: String,
    },
    #[error("unknown name {name:?}; the model declares no input or output called that")]
    UnknownName { name: String },
    #[error("{name} takes {expected} argument(s), and was given {given}")]
    WrongArity {
        name: String,
        expected: usize,
        given: usize,
    },
    #[error("the expression is empty")]
    Empty,
    #[error("the expression nests more than {limit} levels deep")]
    TooDeep { limit: usize },
}

/// How deep an expression may nest.
///
/// A bound rather than a stack overflow. `((((((…))))))` from an API request
/// would otherwise take the process down, and a panic in a worker is a lost job
/// where a rejected specification is a 422.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Power,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Equal,
    NotEqual,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Constant(f64),
    /// An index into the evaluation environment, resolved when compiling.
    Slot(usize),
    Negate(Box<Node>),
    Not(Box<Node>),
    Binary(BinaryOp, Box<Node>, Box<Node>),
    Call(Function, Vec<Node>),
    /// `if(condition, when_true, when_false)`, kept separate from `Call` so
    /// only the taken branch is evaluated.
    Conditional(Box<Node>, Box<Node>, Box<Node>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Function {
    Min,
    Max,
    Abs,
    Sqrt,
    Exp,
    Ln,
    Log10,
    Floor,
    Ceil,
    Round,
    Clamp,
}

impl Function {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "min" => Function::Min,
            "max" => Function::Max,
            "abs" => Function::Abs,
            "sqrt" => Function::Sqrt,
            "exp" => Function::Exp,
            "ln" => Function::Ln,
            "log10" => Function::Log10,
            "floor" => Function::Floor,
            "ceil" => Function::Ceil,
            "round" => Function::Round,
            "clamp" => Function::Clamp,
            _ => return None,
        })
    }

    fn arity(self) -> usize {
        match self {
            Function::Min | Function::Max => 2,
            Function::Clamp => 3,
            _ => 1,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Function::Min => "min",
            Function::Max => "max",
            Function::Abs => "abs",
            Function::Sqrt => "sqrt",
            Function::Exp => "exp",
            Function::Ln => "ln",
            Function::Log10 => "log10",
            Function::Floor => "floor",
            Function::Ceil => "ceil",
            Function::Round => "round",
            Function::Clamp => "clamp",
        }
    }
}

/// The names an expression may use, and where each one lives at evaluation
/// time.
pub type Environment = HashMap<String, usize>;

/// A compiled expression, ready to evaluate a few million times.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    root: Node,
    source: String,
    referenced: Vec<usize>,
}

impl Expression {
    /// Parses and resolves an expression against the names available to it.
    pub fn compile(source: &str, environment: &Environment) -> Result<Self, ExprError> {
        let tokens = tokenise(source)?;
        if tokens.is_empty() {
            return Err(ExprError::Empty);
        }
        let mut parser = Parser {
            tokens,
            position: 0,
            environment,
            referenced: Vec::new(),
            depth: 0,
        };
        let root = parser.parse_expression()?;
        if parser.position < parser.tokens.len() {
            return Err(ExprError::Expected {
                expected: "the end of the expression",
                found: parser.tokens[parser.position].describe(),
            });
        }
        let mut referenced = parser.referenced;
        referenced.sort_unstable();
        referenced.dedup();
        Ok(Self {
            root,
            source: source.to_string(),
            referenced,
        })
    }

    /// The slots this expression actually reads.
    ///
    /// Used by the sensitivity analysis: an input an output never reads has no
    /// influence on it, and reporting a spurious correlation for it — which a
    /// finite sample will always produce some of — is worse than saying nothing.
    pub fn referenced_slots(&self) -> &[usize] {
        &self.referenced
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// How many operations one evaluation costs, near enough.
    ///
    /// Used by the caller to decide whether a run belongs inside a request. A
    /// count of nodes rather than of characters: `a*b` and
    /// `if(a>0, exp(b)/c, 0)` are the same length and not the same work.
    pub fn complexity(&self) -> usize {
        fn count(node: &Node) -> usize {
            1 + match node {
                Node::Constant(_) | Node::Slot(_) => 0,
                Node::Negate(inner) | Node::Not(inner) => count(inner),
                Node::Binary(_, left, right) => count(left) + count(right),
                Node::Call(_, arguments) => arguments.iter().map(count).sum(),
                Node::Conditional(condition, a, b) => count(condition) + count(a) + count(b),
            }
        }
        count(&self.root)
    }

    /// Evaluates against one iteration's values.
    #[inline]
    pub fn evaluate(&self, values: &[f64]) -> f64 {
        evaluate(&self.root, values)
    }
}

fn evaluate(node: &Node, values: &[f64]) -> f64 {
    match node {
        Node::Constant(value) => *value,
        Node::Slot(index) => values[*index],
        Node::Negate(inner) => -evaluate(inner, values),
        Node::Not(inner) => {
            truth(evaluate(inner, values)).map_or(0.0, |t| if t { 0.0 } else { 1.0 })
        }
        Node::Binary(op, left, right) => {
            let a = evaluate(left, values);
            match op {
                // Short-circuit, so `if(x > 0 and 1/x > 2, …)` does not divide
                // by zero to decide it should not have.
                BinaryOp::And => {
                    if truth(a) == Some(false) {
                        return 0.0;
                    }
                    let b = evaluate(right, values);
                    boolean(truth(a) == Some(true) && truth(b) == Some(true))
                }
                BinaryOp::Or => {
                    if truth(a) == Some(true) {
                        return 1.0;
                    }
                    let b = evaluate(right, values);
                    boolean(truth(b) == Some(true))
                }
                _ => {
                    let b = evaluate(right, values);
                    match op {
                        BinaryOp::Add => a + b,
                        BinaryOp::Subtract => a - b,
                        BinaryOp::Multiply => a * b,
                        BinaryOp::Divide => a / b,
                        BinaryOp::Remainder => a % b,
                        BinaryOp::Power => a.powf(b),
                        BinaryOp::Less => boolean(a < b),
                        BinaryOp::LessOrEqual => boolean(a <= b),
                        BinaryOp::Greater => boolean(a > b),
                        BinaryOp::GreaterOrEqual => boolean(a >= b),
                        BinaryOp::Equal => boolean(a == b),
                        BinaryOp::NotEqual => boolean(a != b),
                        BinaryOp::And | BinaryOp::Or => unreachable!("handled above"),
                    }
                }
            }
        }
        Node::Call(function, arguments) => {
            let a = evaluate(&arguments[0], values);
            match function {
                Function::Abs => a.abs(),
                Function::Sqrt => a.sqrt(),
                Function::Exp => a.exp(),
                Function::Ln => a.ln(),
                Function::Log10 => a.log10(),
                Function::Floor => a.floor(),
                Function::Ceil => a.ceil(),
                Function::Round => a.round(),
                Function::Min => a.min(evaluate(&arguments[1], values)),
                Function::Max => a.max(evaluate(&arguments[1], values)),
                Function::Clamp => {
                    let low = evaluate(&arguments[1], values);
                    let high = evaluate(&arguments[2], values);
                    // `clamp` panics if the bounds are the wrong way round, and
                    // a panic here would take a worker down over a typo.
                    if low <= high {
                        a.clamp(low, high)
                    } else {
                        f64::NAN
                    }
                }
            }
        }
        Node::Conditional(condition, when_true, when_false) => {
            match truth(evaluate(condition, values)) {
                Some(true) => evaluate(when_true, values),
                Some(false) => evaluate(when_false, values),
                // A NaN condition is neither true nor false, and picking a
                // branch would silently invent an answer. The iteration becomes
                // NaN, which the engine's validation reports rather than hides.
                None => f64::NAN,
            }
        }
    }
}

/// Truth as this language defines it: zero is false, any other finite number is
/// true, and NaN is neither.
#[inline]
fn truth(value: f64) -> Option<bool> {
    if value.is_nan() {
        None
    } else {
        Some(value != 0.0)
    }
}

#[inline]
fn boolean(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Name(String),
    Symbol(&'static str),
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::Number(value) => value.to_string(),
            Token::Name(name) => name.clone(),
            Token::Symbol(symbol) => (*symbol).to_string(),
        }
    }
}

fn tokenise(source: &str) -> Result<Vec<Token>, ExprError> {
    let characters: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < characters.len() {
        let c = characters[index];
        if c.is_whitespace() {
            index += 1;
            continue;
        }

        if c.is_ascii_digit()
            || (c == '.' && index + 1 < characters.len() && characters[index + 1].is_ascii_digit())
        {
            let start = index;
            while index < characters.len()
                && (characters[index].is_ascii_digit() || characters[index] == '.')
            {
                index += 1;
            }
            // Scientific notation, so 1e6 does not have to be written out.
            if index < characters.len() && (characters[index] == 'e' || characters[index] == 'E') {
                let mut lookahead = index + 1;
                if lookahead < characters.len()
                    && (characters[lookahead] == '+' || characters[lookahead] == '-')
                {
                    lookahead += 1;
                }
                if lookahead < characters.len() && characters[lookahead].is_ascii_digit() {
                    index = lookahead;
                    while index < characters.len() && characters[index].is_ascii_digit() {
                        index += 1;
                    }
                }
            }
            let text: String = characters[start..index].iter().collect();
            let value = text.parse::<f64>().map_err(|_| ExprError::Expected {
                expected: "a number",
                found: text.clone(),
            })?;
            tokens.push(Token::Number(value));
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = index;
            while index < characters.len()
                && (characters[index].is_alphanumeric()
                    || characters[index] == '_'
                    // Swedish letters belong in identifiers: an input called
                    // "omsättning" is the natural name for it here.
                    || characters[index] == '.')
            {
                index += 1;
            }
            tokens.push(Token::Name(characters[start..index].iter().collect()));
            continue;
        }

        let two: String = characters[index..(index + 2).min(characters.len())]
            .iter()
            .collect();
        let symbol = match two.as_str() {
            "<=" => Some("<="),
            ">=" => Some(">="),
            "==" => Some("=="),
            "!=" => Some("!="),
            _ => None,
        };
        if let Some(symbol) = symbol {
            tokens.push(Token::Symbol(symbol));
            index += 2;
            continue;
        }

        let single = match c {
            '+' => "+",
            '-' => "-",
            '*' => "*",
            '/' => "/",
            '%' => "%",
            '^' => "^",
            '(' => "(",
            ')' => ")",
            ',' => ",",
            '<' => "<",
            '>' => ">",
            _ => {
                return Err(ExprError::UnexpectedCharacter {
                    found: c,
                    position: index,
                })
            }
        };
        tokens.push(Token::Symbol(single));
        index += 1;
    }

    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    environment: &'a Environment,
    referenced: Vec<usize>,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(s)) if *s == symbol) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, symbol: &'static str) -> Result<(), ExprError> {
        if self.eat_symbol(symbol) {
            Ok(())
        } else {
            Err(match self.peek() {
                Some(token) => ExprError::Expected {
                    expected: symbol,
                    found: token.describe(),
                },
                None => ExprError::UnexpectedEnd { expected: symbol },
            })
        }
    }

    fn descend(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExprError::TooDeep { limit: MAX_DEPTH });
        }
        Ok(())
    }

    fn parse_expression(&mut self) -> Result<Node, ExprError> {
        self.descend()?;
        let node = self.parse_or();
        self.depth -= 1;
        node
    }

    fn parse_or(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Name(name)) if name == "or") {
            self.position += 1;
            let right = self.parse_and()?;
            left = Node::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_comparison()?;
        while matches!(self.peek(), Some(Token::Name(name)) if name == "and") {
            self.position += 1;
            let right = self.parse_comparison()?;
            left = Node::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Node, ExprError> {
        let left = self.parse_additive()?;
        let op = match self.peek() {
            Some(Token::Symbol("<")) => BinaryOp::Less,
            Some(Token::Symbol("<=")) => BinaryOp::LessOrEqual,
            Some(Token::Symbol(">")) => BinaryOp::Greater,
            Some(Token::Symbol(">=")) => BinaryOp::GreaterOrEqual,
            Some(Token::Symbol("==")) => BinaryOp::Equal,
            Some(Token::Symbol("!=")) => BinaryOp::NotEqual,
            _ => return Ok(left),
        };
        self.position += 1;
        let right = self.parse_additive()?;
        Ok(Node::Binary(op, Box::new(left), Box::new(right)))
    }

    fn parse_additive(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Symbol("+")) => BinaryOp::Add,
                Some(Token::Symbol("-")) => BinaryOp::Subtract,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_multiplicative()?;
            left = Node::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Symbol("*")) => BinaryOp::Multiply,
                Some(Token::Symbol("/")) => BinaryOp::Divide,
                Some(Token::Symbol("%")) => BinaryOp::Remainder,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_unary()?;
            left = Node::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Node, ExprError> {
        if self.eat_symbol("-") {
            self.descend()?;
            let inner = self.parse_unary();
            self.depth -= 1;
            return Ok(Node::Negate(Box::new(inner?)));
        }
        if self.eat_symbol("+") {
            return self.parse_unary();
        }
        if matches!(self.peek(), Some(Token::Name(name)) if name == "not") {
            self.position += 1;
            self.descend()?;
            let inner = self.parse_unary();
            self.depth -= 1;
            return Ok(Node::Not(Box::new(inner?)));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Node, ExprError> {
        let base = self.parse_primary()?;
        if self.eat_symbol("^") {
            // Right-associative: 2^3^2 is 2^(3^2), as everywhere else that has
            // the operator.
            self.descend()?;
            let exponent = self.parse_unary();
            self.depth -= 1;
            return Ok(Node::Binary(
                BinaryOp::Power,
                Box::new(base),
                Box::new(exponent?),
            ));
        }
        Ok(base)
    }

    fn parse_primary(&mut self) -> Result<Node, ExprError> {
        let token = self.peek().cloned().ok_or(ExprError::UnexpectedEnd {
            expected: "a value",
        })?;

        match token {
            Token::Number(value) => {
                self.position += 1;
                Ok(Node::Constant(value))
            }
            Token::Symbol("(") => {
                self.position += 1;
                let inner = self.parse_expression()?;
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Token::Symbol(symbol) => Err(ExprError::Expected {
                expected: "a value",
                found: symbol.to_string(),
            }),
            Token::Name(name) => {
                self.position += 1;

                if self.eat_symbol("(") {
                    let mut arguments = Vec::new();
                    if !self.eat_symbol(")") {
                        loop {
                            arguments.push(self.parse_expression()?);
                            if self.eat_symbol(",") {
                                continue;
                            }
                            self.expect_symbol(")")?;
                            break;
                        }
                    }

                    if name == "if" {
                        if arguments.len() != 3 {
                            return Err(ExprError::WrongArity {
                                name,
                                expected: 3,
                                given: arguments.len(),
                            });
                        }
                        let mut drained = arguments.into_iter();
                        return Ok(Node::Conditional(
                            Box::new(drained.next().expect("three arguments")),
                            Box::new(drained.next().expect("three arguments")),
                            Box::new(drained.next().expect("three arguments")),
                        ));
                    }

                    let function = Function::parse(&name)
                        .ok_or(ExprError::UnknownName { name: name.clone() })?;
                    if arguments.len() != function.arity() {
                        return Err(ExprError::WrongArity {
                            name: function.name().to_string(),
                            expected: function.arity(),
                            given: arguments.len(),
                        });
                    }
                    return Ok(Node::Call(function, arguments));
                }

                // Constants a model is allowed to write by name. Deliberately
                // few: anything else has to be declared as an input, where it
                // gets a source and a description.
                match name.as_str() {
                    "pi" => return Ok(Node::Constant(std::f64::consts::PI)),
                    "e" => return Ok(Node::Constant(std::f64::consts::E)),
                    "true" => return Ok(Node::Constant(1.0)),
                    "false" => return Ok(Node::Constant(0.0)),
                    _ => {}
                }

                let slot = *self
                    .environment
                    .get(&name)
                    .ok_or(ExprError::UnknownName { name: name.clone() })?;
                self.referenced.push(slot);
                Ok(Node::Slot(slot))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(names: &[&str]) -> Environment {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ((*name).to_string(), index))
            .collect()
    }

    fn eval(source: &str, names: &[&str], values: &[f64]) -> f64 {
        Expression::compile(source, &environment(names))
            .expect("compiles")
            .evaluate(values)
    }

    #[test]
    fn arithmetic_follows_the_usual_precedence() {
        assert_eq!(eval("2 + 3 * 4", &[], &[]), 14.0);
        assert_eq!(eval("(2 + 3) * 4", &[], &[]), 20.0);
        assert_eq!(eval("10 - 2 - 3", &[], &[]), 5.0);
        assert_eq!(eval("2 ^ 3 ^ 2", &[], &[]), 512.0);
        assert_eq!(eval("-2 ^ 2", &[], &[]), -4.0);
        assert_eq!(eval("7 % 3", &[], &[]), 1.0);
    }

    #[test]
    fn the_worked_examples_from_the_specification() {
        // Revenue = Customers × Average Revenue
        assert_eq!(
            eval(
                "customers * average_revenue",
                &["customers", "average_revenue"],
                &[1200.0, 850.0]
            ),
            1_020_000.0
        );
        // Profit = Revenue − Costs
        assert_eq!(
            eval("revenue - costs", &["revenue", "costs"], &[100.0, 40.0]),
            60.0
        );
        // ROI = (Return − Investment) / Investment
        assert_eq!(
            eval(
                "(gross_return - investment) / investment",
                &["gross_return", "investment"],
                &[150.0, 100.0]
            ),
            0.5
        );
    }

    #[test]
    fn functions_work_and_check_their_arity() {
        assert_eq!(eval("max(3, 9)", &[], &[]), 9.0);
        assert_eq!(eval("min(3, 9)", &[], &[]), 3.0);
        assert_eq!(eval("abs(0 - 5)", &[], &[]), 5.0);
        assert_eq!(eval("clamp(15, 0, 10)", &[], &[]), 10.0);
        assert_eq!(eval("round(2.6)", &[], &[]), 3.0);
        assert!((eval("ln(e)", &[], &[]) - 1.0).abs() < 1e-12);

        let error = Expression::compile("max(1)", &environment(&[])).unwrap_err();
        assert!(matches!(
            error,
            ExprError::WrongArity {
                expected: 2,
                given: 1,
                ..
            }
        ));
    }

    #[test]
    fn conditionals_choose_a_branch() {
        assert_eq!(eval("if(1 > 0, 10, 20)", &[], &[]), 10.0);
        assert_eq!(eval("if(1 < 0, 10, 20)", &[], &[]), 20.0);
        assert_eq!(
            eval("if(profit > 0, profit * 0.206, 0)", &["profit"], &[1000.0]),
            206.0
        );
        assert_eq!(
            eval("if(profit > 0, profit * 0.206, 0)", &["profit"], &[-50.0]),
            0.0
        );
    }

    #[test]
    fn a_conditional_only_evaluates_the_branch_it_takes() {
        // The untaken branch divides by zero. If both branches were evaluated
        // the result would be NaN rather than 1.
        assert_eq!(eval("if(0, 1 / 0, 1)", &[], &[]), 1.0);
    }

    #[test]
    fn and_short_circuits() {
        // The right-hand side would be NaN; `and` must not reach it.
        assert_eq!(eval("if(0 and (0/0), 1, 2)", &[], &[]), 2.0);
    }

    #[test]
    fn a_nan_condition_produces_nan_rather_than_a_guess() {
        assert!(eval("if(0/0, 1, 2)", &[], &[]).is_nan());
    }

    #[test]
    fn an_unknown_name_is_rejected_at_compile_time() {
        let error =
            Expression::compile("revenue - cost", &environment(&["revenue", "costs"])).unwrap_err();
        assert_eq!(
            error,
            ExprError::UnknownName {
                name: "cost".into()
            }
        );
    }

    #[test]
    fn the_referenced_slots_are_exactly_those_read() {
        let compiled =
            Expression::compile("a + c * 2", &environment(&["a", "b", "c", "d"])).unwrap();
        assert_eq!(compiled.referenced_slots(), &[0, 2]);
    }

    #[test]
    fn malformed_expressions_are_rejected_rather_than_guessed() {
        for source in ["2 +", "(2 + 3", "2 3", "* 4", "", "  ", "2 + @"] {
            assert!(
                Expression::compile(source, &environment(&[])).is_err(),
                "{source:?} was accepted"
            );
        }
    }

    #[test]
    fn deep_nesting_is_bounded_rather_than_fatal() {
        let source = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        let error = Expression::compile(&source, &environment(&[])).unwrap_err();
        assert!(matches!(error, ExprError::TooDeep { .. }));
    }

    #[test]
    fn complexity_counts_work_rather_than_characters() {
        let environment = environment(&["a", "b", "c"]);
        let simple = Expression::compile("a * b", &environment).unwrap();
        let branching = Expression::compile("if(a > 0, exp(b) / c, 0)", &environment).unwrap();
        assert!(
            branching.complexity() > simple.complexity() * 2,
            "{} against {}",
            branching.complexity(),
            simple.complexity()
        );
        assert_eq!(
            Expression::compile("1", &environment).unwrap().complexity(),
            1
        );
    }

    #[test]
    fn swedish_identifiers_work() {
        assert_eq!(
            eval(
                "omsättning - kostnader",
                &["omsättning", "kostnader"],
                &[500.0, 200.0]
            ),
            300.0
        );
    }

    #[test]
    fn scientific_notation_parses() {
        assert_eq!(eval("1e6", &[], &[]), 1_000_000.0);
        assert_eq!(eval("2.5e-3", &[], &[]), 0.0025);
    }

    #[test]
    fn division_by_zero_is_not_hidden() {
        // It produces an infinity, which the engine's validation counts and
        // reports. What it must not do is quietly become zero.
        assert!(eval("1 / 0", &[], &[]).is_infinite());
        assert!(eval("0 / 0", &[], &[]).is_nan());
    }

    #[test]
    fn clamp_with_reversed_bounds_is_nan_rather_than_a_panic() {
        assert!(eval("clamp(5, 10, 1)", &[], &[]).is_nan());
    }
}
