use std::cell::Cell;

use oxc_allocator::Box;
use oxc_ast::ast::*;
use oxc_diagnostics::Result;
use oxc_span::{Atom, Span};
use oxc_syntax::{
    number::{BigintBase, NumberBase},
    operator::BinaryOperator,
    precedence::Precedence,
};

use super::{
    grammar::CoverGrammar,
    list::{ArrayExpressionList, CallArguments, SequenceExpressionList},
    operator::{
        kind_to_precedence, map_assignment_operator, map_binary_operator, map_logical_operator,
        map_unary_operator, map_update_operator,
    },
};
use crate::{
    diagnostics,
    lexer::{parse_big_int, parse_float, parse_int, Kind},
    list::SeparatedList,
    Context, ParserImpl,
};

impl<'a> ParserImpl<'a> {
    pub(crate) fn parse_paren_expression(&mut self) -> Result<Expression<'a>> {
        self.expect(Kind::LParen)?;
        let expression = self.parse_expr()?;
        self.expect(Kind::RParen)?;
        Ok(expression)
    }

    /// Section [Expression](https://tc39.es/ecma262/#sec-ecmascript-language-expressions)
    pub(crate) fn parse_expr(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();

        let has_decorator = self.ctx.has_decorator();
        if has_decorator {
            self.ctx = self.ctx.and_decorator(false);
        }

        let lhs = self.parse_assignment_expression_or_higher()?;
        if !self.at(Kind::Comma) {
            return Ok(lhs);
        }

        let expr = self.parse_sequence_expression(span, lhs)?;

        if has_decorator {
            self.ctx = self.ctx.and_decorator(true);
        }

        Ok(expr)
    }

    /// `PrimaryExpression`: Identifier Reference
    pub(crate) fn parse_identifier_expression(&mut self) -> Result<Expression<'a>> {
        let ident = self.parse_identifier_reference()?;
        Ok(self.ast.identifier_reference_expression(ident))
    }

    pub(crate) fn parse_identifier_reference(&mut self) -> Result<IdentifierReference<'a>> {
        // allow `await` and `yield`, let semantic analysis report error
        if !self.cur_kind().is_identifier_reference(false, false) {
            return Err(self.unexpected());
        }
        let (span, name) = self.parse_identifier_kind(Kind::Ident);
        self.check_identifier(span, &name);
        Ok(IdentifierReference::new(span, name))
    }

    /// `BindingIdentifier` : Identifier
    pub(crate) fn parse_binding_identifier(&mut self) -> Result<BindingIdentifier<'a>> {
        if !self.cur_kind().is_binding_identifier() {
            return Err(self.unexpected());
        }
        let (span, name) = self.parse_identifier_kind(Kind::Ident);
        self.check_identifier(span, &name);
        Ok(BindingIdentifier { span, name, symbol_id: Cell::default() })
    }

    pub(crate) fn parse_label_identifier(&mut self) -> Result<LabelIdentifier<'a>> {
        if !self.cur_kind().is_label_identifier(self.ctx.has_yield(), self.ctx.has_await()) {
            return Err(self.unexpected());
        }
        let (span, name) = self.parse_identifier_kind(Kind::Ident);
        self.check_identifier(span, &name);
        Ok(LabelIdentifier { span, name })
    }

    pub(crate) fn parse_identifier_name(&mut self) -> Result<IdentifierName<'a>> {
        if !self.cur_kind().is_identifier_name() {
            return Err(self.unexpected());
        }
        let (span, name) = self.parse_identifier_kind(Kind::Ident);
        Ok(IdentifierName { span, name })
    }

    /// Parse keyword kind as identifier
    pub(crate) fn parse_keyword_identifier(&mut self, kind: Kind) -> IdentifierName<'a> {
        let (span, name) = self.parse_identifier_kind(kind);
        IdentifierName { span, name }
    }

    #[inline]
    pub(crate) fn parse_identifier_kind(&mut self, kind: Kind) -> (Span, Atom<'a>) {
        let span = self.start_span();
        let name = self.cur_string();
        self.bump_remap(kind);
        (self.end_span(span), Atom::from(name))
    }

    pub(crate) fn check_identifier(&mut self, span: Span, name: &str) {
        // It is a Syntax Error if this production has an [Await] parameter.
        if self.ctx.has_await() && name == "await" {
            self.error(diagnostics::identifier_async("await", span));
        }
        // It is a Syntax Error if this production has a [Yield] parameter.
        if self.ctx.has_yield() && name == "yield" {
            self.error(diagnostics::identifier_generator("yield", span));
        }
    }

    /// Section [PrivateIdentifier](https://tc39.es/ecma262/#prod-PrivateIdentifier)
    /// `PrivateIdentifier` ::
    ///     # `IdentifierName`
    /// # Panics
    pub(crate) fn parse_private_identifier(&mut self) -> PrivateIdentifier<'a> {
        let span = self.start_span();
        let name = Atom::from(self.cur_string());
        self.bump_any();
        PrivateIdentifier { span: self.end_span(span), name }
    }

    /// Section [Primary Expression](https://tc39.es/ecma262/#sec-primary-expression)
    /// `PrimaryExpression`[Yield, Await] :
    ///     this
    ///     `IdentifierReference`[?Yield, ?Await]
    ///     Literal
    ///     `ArrayLiteral`[?Yield, ?Await]
    ///     `ObjectLiteral`[?Yield, ?Await]
    ///     `FunctionExpression`
    ///     `ClassExpression`[?Yield, ?Await]
    ///     `GeneratorExpression`
    ///     `AsyncFunctionExpression`
    ///     `AsyncGeneratorExpression`
    ///     `RegularExpressionLiteral`
    ///     `TemplateLiteral`[?Yield, ?Await, ~Tagged]
    ///     `CoverParenthesizedExpressionAndArrowParameterList`[?Yield, ?Await]
    fn parse_primary_expression(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();

        if self.at(Kind::At) {
            self.eat_decorators()?;
        }

        // FunctionExpression, GeneratorExpression
        // AsyncFunctionExpression, AsyncGeneratorExpression
        if self.at_function_with_async() {
            let r#async = self.eat(Kind::Async);
            return self.parse_function_expression(span, r#async);
        }

        match &self.cur_kind() {
            Kind::Ident => self.parse_identifier_expression(), // fast path, keywords are checked at the end
            // Literal, RegularExpressionLiteral
            kind if kind.is_literal() => self.parse_literal_expression(),
            // ArrayLiteral
            Kind::LBrack => self.parse_array_expression(),
            // ObjectLiteral
            Kind::LCurly => self.parse_object_expression(),
            // ClassExpression
            Kind::Class => self.parse_class_expression(),
            // This
            Kind::This => Ok(self.parse_this_expression()),
            // TemplateLiteral
            Kind::NoSubstitutionTemplate | Kind::TemplateHead => {
                self.parse_template_literal_expression(false)
            }
            Kind::New => self.parse_new_expression(),
            Kind::Super => Ok(self.parse_super()),
            Kind::Import => {
                let span = self.start_span();
                let identifier = self.parse_keyword_identifier(Kind::Import);
                match self.cur_kind() {
                    Kind::Dot => self.parse_meta_property(span, identifier),
                    Kind::LParen => self.parse_import_expression(span),
                    _ => Err(self.unexpected()),
                }
            }
            Kind::LParen => self.parse_parenthesized_expression(span),
            Kind::Slash | Kind::SlashEq => {
                let literal = self.parse_literal_regexp();
                Ok(self.ast.literal_regexp_expression(literal))
            }
            // JSXElement, JSXFragment
            Kind::LAngle if self.source_type.is_jsx() => self.parse_jsx_expression(),
            _ => self.parse_identifier_expression(),
        }
    }

    fn parse_parenthesized_expression(&mut self, span: Span) -> Result<Expression<'a>> {
        let list = self.context(Context::In, Context::Decorator, SequenceExpressionList::parse)?;

        let mut expressions = list.elements;
        let paren_span = self.end_span(span);

        if expressions.is_empty() {
            return Err(diagnostics::empty_parenthesized_expression(paren_span));
        }

        // ParenthesizedExpression is from acorn --preserveParens
        let expression = if expressions.len() == 1 {
            expressions.remove(0)
        } else {
            self.ast.sequence_expression(
                Span::new(paren_span.start + 1, paren_span.end - 1),
                expressions,
            )
        };

        Ok(if self.preserve_parens {
            self.ast.parenthesized_expression(paren_span, expression)
        } else {
            expression
        })
    }

    /// Section 13.2.2 This Expression
    fn parse_this_expression(&mut self) -> Expression<'a> {
        let span = self.start_span();
        self.bump_any();
        self.ast.this_expression(self.end_span(span))
    }

    /// [Literal Expression](https://tc39.es/ecma262/#prod-Literal)
    /// parses string | true | false | null | number
    pub(crate) fn parse_literal_expression(&mut self) -> Result<Expression<'a>> {
        match self.cur_kind() {
            Kind::Str => self
                .parse_literal_string()
                .map(|literal| self.ast.literal_string_expression(literal)),
            Kind::True | Kind::False => self
                .parse_literal_boolean()
                .map(|literal| self.ast.literal_boolean_expression(literal)),
            Kind::Null => {
                let literal = self.parse_literal_null();
                Ok(self.ast.literal_null_expression(literal))
            }
            kind if kind.is_number() => {
                if self.cur_src().ends_with('n') {
                    self.parse_literal_bigint()
                        .map(|literal| self.ast.literal_bigint_expression(literal))
                } else {
                    self.parse_literal_number()
                        .map(|literal| self.ast.literal_number_expression(literal))
                }
            }
            _ => Err(self.unexpected()),
        }
    }

    pub(crate) fn parse_literal_boolean(&mut self) -> Result<BooleanLiteral> {
        let span = self.start_span();
        let value = match self.cur_kind() {
            Kind::True => true,
            Kind::False => false,
            _ => return Err(self.unexpected()),
        };
        self.bump_any();
        Ok(BooleanLiteral { span: self.end_span(span), value })
    }

    pub(crate) fn parse_literal_null(&mut self) -> NullLiteral {
        let span = self.start_span();
        self.bump_any(); // bump `null`
        NullLiteral { span: self.end_span(span) }
    }

    pub(crate) fn parse_literal_number(&mut self) -> Result<NumericLiteral<'a>> {
        let span = self.start_span();
        let token = self.cur_token();
        let src = self.cur_src();
        let value = match token.kind {
            Kind::Decimal | Kind::Binary | Kind::Octal | Kind::Hex => {
                parse_int(src, token.kind, token.has_separator())
            }
            Kind::Float | Kind::PositiveExponential | Kind::NegativeExponential => {
                parse_float(src, token.has_separator())
            }
            _ => unreachable!(),
        }
        .map_err(|err| diagnostics::invalid_number(err, token.span()))?;
        let base = match token.kind {
            Kind::Decimal => NumberBase::Decimal,
            Kind::Float => NumberBase::Float,
            Kind::Binary => NumberBase::Binary,
            Kind::Octal => NumberBase::Octal,
            Kind::Hex => NumberBase::Hex,
            Kind::PositiveExponential | Kind::NegativeExponential => {
                if value.fract() == 0.0 {
                    NumberBase::Decimal
                } else {
                    NumberBase::Float
                }
            }
            _ => return Err(self.unexpected()),
        };
        self.bump_any();
        Ok(NumericLiteral::new(self.end_span(span), value, src, base))
    }

    pub(crate) fn parse_literal_bigint(&mut self) -> Result<BigIntLiteral<'a>> {
        let span = self.start_span();
        let base = match self.cur_kind() {
            Kind::Decimal => BigintBase::Decimal,
            Kind::Binary => BigintBase::Binary,
            Kind::Octal => BigintBase::Octal,
            Kind::Hex => BigintBase::Hex,
            _ => return Err(self.unexpected()),
        };
        let token = self.cur_token();
        let raw = self.cur_src();
        let src = raw.strip_suffix('n').unwrap();
        let _value = parse_big_int(src, token.kind, token.has_separator())
            .map_err(|err| diagnostics::invalid_number(err, token.span()))?;
        self.bump_any();
        Ok(self.ast.bigint_literal(self.end_span(span), Atom::from(raw), base))
    }

    pub(crate) fn parse_literal_regexp(&mut self) -> RegExpLiteral<'a> {
        let span = self.start_span();

        // split out pattern
        let (pattern_end, flags) = self.read_regex();
        let pattern_start = self.cur_token().start + 1; // +1 to exclude `/`
        let pattern = &self.source_text[pattern_start as usize..pattern_end as usize];

        self.bump_any();
        self.ast.reg_exp_literal(self.end_span(span), pattern, flags)
    }

    pub(crate) fn parse_literal_string(&mut self) -> Result<StringLiteral<'a>> {
        if !self.at(Kind::Str) {
            return Err(self.unexpected());
        }
        let value = self.cur_string();
        let span = self.start_span();
        self.bump_any();
        Ok(StringLiteral { span: self.end_span(span), value: value.into() })
    }

    /// Section [Array Expression](https://tc39.es/ecma262/#prod-ArrayLiteral)
    /// `ArrayLiteral`[Yield, Await]:
    ///     [ Elision opt ]
    ///     [ `ElementList`[?Yield, ?Await] ]
    ///     [ `ElementList`[?Yield, ?Await] , Elisionopt ]
    pub(crate) fn parse_array_expression(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();
        let list = self.context(Context::In, Context::empty(), ArrayExpressionList::parse)?;
        Ok(self.ast.array_expression(self.end_span(span), list.elements, list.trailing_comma))
    }

    /// Elision :
    ///     ,
    ///    Elision ,
    pub(crate) fn parse_elision(&mut self) -> ArrayExpressionElement<'a> {
        ArrayExpressionElement::Elision(Elision { span: self.cur_token().span() })
    }

    /// Section [Template Literal](https://tc39.es/ecma262/#prod-TemplateLiteral)
    /// `TemplateLiteral`[Yield, Await, Tagged] :
    ///     `NoSubstitutionTemplate`
    ///     `SubstitutionTemplate`[?Yield, ?Await, ?Tagged]
    fn parse_template_literal(&mut self, tagged: bool) -> Result<TemplateLiteral<'a>> {
        let span = self.start_span();
        let mut expressions = self.ast.new_vec();
        let mut quasis = self.ast.new_vec();
        match self.cur_kind() {
            Kind::NoSubstitutionTemplate => {
                quasis.push(self.parse_template_element(tagged));
            }
            Kind::TemplateHead => {
                quasis.push(self.parse_template_element(tagged));
                // TemplateHead Expression[+In, ?Yield, ?Await]
                let expr = self.context(Context::In, Context::empty(), Self::parse_expr)?;
                expressions.push(expr);
                self.re_lex_template_substitution_tail();
                loop {
                    match self.cur_kind() {
                        Kind::Eof => self.expect(Kind::TemplateTail)?,
                        Kind::TemplateTail => {
                            quasis.push(self.parse_template_element(tagged));
                            break;
                        }
                        Kind::TemplateMiddle => {
                            quasis.push(self.parse_template_element(tagged));
                        }
                        _ => {
                            // TemplateMiddle Expression[+In, ?Yield, ?Await]
                            let expr =
                                self.context(Context::In, Context::empty(), Self::parse_expr)?;
                            expressions.push(expr);
                            self.re_lex_template_substitution_tail();
                        }
                    }
                }
            }
            _ => unreachable!("parse_template_literal"),
        }
        Ok(TemplateLiteral { span: self.end_span(span), quasis, expressions })
    }

    fn parse_template_literal_expression(&mut self, tagged: bool) -> Result<Expression<'a>> {
        self.parse_template_literal(tagged)
            .map(|template_literal| self.ast.template_literal_expression(template_literal))
    }

    fn parse_tagged_template(
        &mut self,
        span: Span,
        lhs: Expression<'a>,
        in_optional_chain: bool,
        type_parameters: Option<Box<'a, TSTypeParameterInstantiation<'a>>>,
    ) -> Result<Expression<'a>> {
        let quasi = self.parse_template_literal(true)?;
        let span = self.end_span(span);
        // OptionalChain :
        //   ?. TemplateLiteral
        //   OptionalChain TemplateLiteral
        // It is a Syntax Error if any source text is matched by this production.
        // <https://tc39.es/ecma262/#sec-left-hand-side-expressions-static-semantics-early-errors>
        if in_optional_chain {
            self.error(diagnostics::optional_chain_tagged_template(quasi.span));
        }
        Ok(self.ast.tagged_template_expression(span, lhs, quasi, type_parameters))
    }

    pub(crate) fn parse_template_element(&mut self, tagged: bool) -> TemplateElement<'a> {
        let span = self.start_span();
        let cur_kind = self.cur_kind();
        let end_offset: u32 = match cur_kind {
            Kind::TemplateHead | Kind::TemplateMiddle => 2,
            Kind::NoSubstitutionTemplate | Kind::TemplateTail => 1,
            _ => unreachable!(),
        };

        // `cooked = None` when template literal has invalid escape sequence
        // This is matched by `is_valid_escape_sequence` in `Lexer::read_template_literal`
        let cooked = self.cur_template_string();

        let cur_src = self.cur_src();
        let raw = &cur_src[1..cur_src.len() - end_offset as usize];
        let raw = Atom::from(if cooked.is_some() && raw.contains('\r') {
            self.ast.new_str(raw.replace("\r\n", "\n").replace('\r', "\n").as_str())
        } else {
            raw
        });

        self.bump_any();

        let mut span = self.end_span(span);
        span.start += 1;
        span.end -= end_offset;

        if !tagged && cooked.is_none() {
            self.error(diagnostics::template_literal(span));
        }

        let tail = matches!(cur_kind, Kind::TemplateTail | Kind::NoSubstitutionTemplate);
        TemplateElement {
            span,
            tail,
            value: TemplateElementValue { raw, cooked: cooked.map(Atom::from) },
        }
    }

    /// Section 13.3 Meta Property
    fn parse_meta_property(
        &mut self,
        span: Span,
        meta: IdentifierName<'a>,
    ) -> Result<Expression<'a>> {
        self.bump_any(); // bump `.`
        let property = match self.cur_kind() {
            Kind::Meta => self.parse_keyword_identifier(Kind::Meta),
            Kind::Target => self.parse_keyword_identifier(Kind::Target),
            _ => self.parse_identifier_name()?,
        };
        let span = self.end_span(span);
        Ok(self.ast.meta_property(span, meta, property))
    }

    /// Section 13.3 Left-Hand-Side Expression
    pub(crate) fn parse_lhs_expression_or_higher(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();
        let mut in_optional_chain = false;
        let lhs = self.parse_member_expression_or_higher(&mut in_optional_chain)?;
        let lhs = self.parse_call_expression_rest(span, lhs, &mut in_optional_chain)?;
        if in_optional_chain {
            let span = self.end_span(span);
            Ok(self.map_to_chain_expression(span, lhs))
        } else {
            Ok(lhs)
        }
    }

    fn map_to_chain_expression(&mut self, span: Span, expr: Expression<'a>) -> Expression<'a> {
        match expr {
            match_member_expression!(Expression) => {
                let member_expr = MemberExpression::try_from(expr).unwrap();
                self.ast.chain_expression(span, ChainElement::from(member_expr))
            }
            Expression::CallExpression(result) => {
                self.ast.chain_expression(span, ChainElement::CallExpression(result))
            }
            expr => expr,
        }
    }

    /// Section 13.3 Member Expression
    fn parse_member_expression_or_higher(
        &mut self,
        in_optional_chain: &mut bool,
    ) -> Result<Expression<'a>> {
        let span = self.start_span();
        self.parse_primary_expression()
            .and_then(|lhs| self.parse_member_expression_rest(span, lhs, in_optional_chain))
    }

    /// Section 13.3 Super Call
    fn parse_super(&mut self) -> Expression<'a> {
        let span = self.start_span();
        self.bump_any(); // bump `super`
        let span = self.end_span(span);

        // The `super` keyword can appear at below:
        // SuperProperty:
        //     super [ Expression ]
        //     super . IdentifierName
        // SuperCall:
        //     super ( Arguments )
        if !matches!(self.cur_kind(), Kind::Dot | Kind::LBrack | Kind::LParen) {
            self.error(diagnostics::unexpected_super(span));
        }

        self.ast.super_(span)
    }

    /// parse rhs of a member expression, starting from lhs
    fn parse_member_expression_rest(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        in_optional_chain: &mut bool,
    ) -> Result<Expression<'a>> {
        let mut lhs = lhs;
        loop {
            lhs = match self.cur_kind() {
                // computed member expression is not allowed in decorator
                // class C { @dec ["1"]() { } }
                //                ^
                Kind::LBrack if !self.ctx.has_decorator() => {
                    self.parse_computed_member_expression(lhs_span, lhs, false)?
                }
                Kind::Dot => self.parse_static_member_expression(lhs_span, lhs, false)?,
                Kind::QuestionDot => {
                    *in_optional_chain = true;
                    match self.peek_kind() {
                        Kind::LBrack if !self.ctx.has_decorator() => {
                            self.bump_any(); // bump `?.`
                            self.parse_computed_member_expression(lhs_span, lhs, true)?
                        }
                        Kind::PrivateIdentifier => {
                            self.parse_static_member_expression(lhs_span, lhs, true)?
                        }
                        kind if kind.is_identifier_name() => {
                            self.parse_static_member_expression(lhs_span, lhs, true)?
                        }
                        _ => break,
                    }
                }
                Kind::Bang if !self.cur_token().is_on_new_line && self.ts_enabled() => {
                    self.bump_any();
                    self.ast.ts_non_null_expression(self.end_span(lhs_span), lhs)
                }
                kind if kind.is_template_start_of_tagged_template() => {
                    let (expr, type_parameters) =
                        if let Expression::TSInstantiationExpression(instantiation_expr) = lhs {
                            let expr = instantiation_expr.unbox();
                            (expr.expression, Some(expr.type_parameters))
                        } else {
                            (lhs, None)
                        };
                    self.parse_tagged_template(lhs_span, expr, *in_optional_chain, type_parameters)?
                }
                Kind::LAngle | Kind::ShiftLeft => {
                    if let Ok(Some(arguments)) =
                        self.try_parse(Self::parse_ts_type_arguments_in_expression)
                    {
                        lhs = self.ast.ts_instantiation_expression(
                            self.end_span(lhs_span),
                            lhs,
                            arguments,
                        );
                        continue;
                    }
                    break;
                }
                _ => break,
            };
        }
        Ok(lhs)
    }

    /// Section 13.3 `MemberExpression`
    /// static member `a.b`
    fn parse_static_member_expression(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        optional: bool,
    ) -> Result<Expression<'a>> {
        self.bump_any(); // advance `.` or `?.`
        if self.cur_kind() == Kind::PrivateIdentifier {
            let private_ident = self.parse_private_identifier();
            Ok(self.ast.private_field_expression(
                self.end_span(lhs_span),
                lhs,
                private_ident,
                optional,
            ))
        } else {
            let ident = self.parse_identifier_name()?;
            Ok(self.ast.static_member_expression(self.end_span(lhs_span), lhs, ident, optional))
        }
    }

    /// Section 13.3 `MemberExpression`
    /// `MemberExpression`[Yield, Await] :
    ///   `MemberExpression`[?Yield, ?Await] [ Expression[+In, ?Yield, ?Await] ]
    fn parse_computed_member_expression(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        optional: bool,
    ) -> Result<Expression<'a>> {
        self.bump_any(); // advance `[`
        let property = self.context(Context::In, Context::empty(), Self::parse_expr)?;
        self.expect(Kind::RBrack)?;
        Ok(self.ast.computed_member_expression(self.end_span(lhs_span), lhs, property, optional))
    }

    /// [NewExpression](https://tc39.es/ecma262/#sec-new-operator)
    fn parse_new_expression(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();
        let identifier = self.parse_keyword_identifier(Kind::New);
        if self.at(Kind::Dot) {
            return self.parse_meta_property(span, identifier);
        }
        let rhs_span = self.start_span();

        let mut optional = false;
        let mut callee = self.parse_member_expression_or_higher(&mut optional)?;

        let mut type_parameter = None;
        if let Expression::TSInstantiationExpression(instantiation_expr) = callee {
            let instantiation_expr = instantiation_expr.unbox();
            type_parameter.replace(instantiation_expr.type_parameters);
            callee = instantiation_expr.expression;
        }

        // parse `new ident` without arguments
        let arguments = if self.at(Kind::LParen) {
            // ArgumentList[Yield, Await] :
            //   AssignmentExpression[+In, ?Yield, ?Await]
            self.context(Context::In, Context::empty(), CallArguments::parse)?.elements
        } else {
            self.ast.new_vec()
        };

        if matches!(callee, Expression::ImportExpression(_)) {
            self.error(diagnostics::new_dynamic_import(self.end_span(rhs_span)));
        }

        let span = self.end_span(span);

        if optional {
            self.error(diagnostics::new_optional_chain(span));
        }

        Ok(self.ast.new_expression(span, callee, arguments, type_parameter))
    }

    /// Section 13.3 Call Expression
    fn parse_call_expression_rest(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        in_optional_chain: &mut bool,
    ) -> Result<Expression<'a>> {
        let mut lhs = lhs;
        loop {
            let mut type_arguments = None;
            lhs = self.parse_member_expression_rest(lhs_span, lhs, in_optional_chain)?;
            let optional_call = self.eat(Kind::QuestionDot);
            *in_optional_chain = if optional_call { true } else { *in_optional_chain };

            if optional_call {
                if let Ok(Some(args)) = self.try_parse(Self::parse_ts_type_arguments_in_expression)
                {
                    type_arguments = Some(args);
                }
                if self.cur_kind().is_template_start_of_tagged_template() {
                    lhs =
                        self.parse_tagged_template(lhs_span, lhs, optional_call, type_arguments)?;
                    continue;
                }
            }

            if type_arguments.is_some() || self.at(Kind::LParen) {
                if let Expression::TSInstantiationExpression(expr) = lhs {
                    let expr = expr.unbox();
                    type_arguments.replace(expr.type_parameters);
                    lhs = expr.expression;
                }

                lhs =
                    self.parse_call_arguments(lhs_span, lhs, optional_call, type_arguments.take())?;
                continue;
            }
            break;
        }

        Ok(lhs)
    }

    fn parse_call_arguments(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        optional: bool,
        type_parameters: Option<Box<'a, TSTypeParameterInstantiation<'a>>>,
    ) -> Result<Expression<'a>> {
        // ArgumentList[Yield, Await] :
        //   AssignmentExpression[+In, ?Yield, ?Await]
        let call_arguments = self.context(Context::In, Context::Decorator, CallArguments::parse)?;
        Ok(self.ast.call_expression(
            self.end_span(lhs_span),
            lhs,
            call_arguments.elements,
            optional,
            type_parameters,
        ))
    }

    /// Section 13.4 Update Expression
    fn parse_update_expression(&mut self, lhs_span: Span) -> Result<Expression<'a>> {
        let kind = self.cur_kind();
        // ++ -- prefix update expressions
        if kind.is_update_operator() {
            let operator = map_update_operator(kind);
            self.bump_any();
            let argument = self.parse_unary_expression_or_higher(lhs_span)?;
            let argument = SimpleAssignmentTarget::cover(argument, self)?;
            return Ok(self.ast.update_expression(
                self.end_span(lhs_span),
                operator,
                true,
                argument,
            ));
        }

        if self.source_type.is_jsx()
            && kind == Kind::LAngle
            && self.peek_kind().is_identifier_name()
        {
            return self.parse_jsx_expression();
        }

        let span = self.start_span();
        let lhs = self.parse_lhs_expression_or_higher()?;
        // ++ -- postfix update expressions
        if self.cur_kind().is_update_operator() && !self.cur_token().is_on_new_line {
            let operator = map_update_operator(self.cur_kind());
            self.bump_any();
            let lhs = SimpleAssignmentTarget::cover(lhs, self)?;
            return Ok(self.ast.update_expression(self.end_span(span), operator, false, lhs));
        }
        Ok(lhs)
    }

    /// Section 13.5 Unary Expression
    pub(crate) fn parse_unary_expression_or_higher(
        &mut self,
        lhs_span: Span,
    ) -> Result<Expression<'a>> {
        // ++ -- prefix update expressions
        if self.is_update_expression() {
            return self.parse_update_expression(lhs_span);
        }
        self.parse_simple_unary_expression(lhs_span)
    }

    pub(crate) fn parse_simple_unary_expression(
        &mut self,
        lhs_span: Span,
    ) -> Result<Expression<'a>> {
        match self.cur_kind() {
            kind if kind.is_unary_operator() => self.parse_unary_expression(),
            Kind::LAngle => {
                if self.source_type.is_jsx() {
                    return self.parse_jsx_expression();
                }
                if self.ts_enabled() {
                    return self.parse_ts_type_assertion();
                }
                Err(self.unexpected())
            }
            Kind::Await if self.is_await_expression() => self.parse_await_expression(lhs_span),
            _ => self.parse_update_expression(lhs_span),
        }
    }

    fn parse_unary_expression(&mut self) -> Result<Expression<'a>> {
        let span = self.start_span();
        let operator = map_unary_operator(self.cur_kind());
        self.bump_any();
        let argument = self.parse_simple_unary_expression(span)?;
        Ok(self.ast.unary_expression(self.end_span(span), operator, argument))
    }

    pub(crate) fn parse_binary_expression_or_higher(
        &mut self,
        lhs_precedence: Precedence,
    ) -> Result<Expression<'a>> {
        let lhs_span = self.start_span();

        let lhs = if self.ctx.has_in() && self.at(Kind::PrivateIdentifier) {
            let left = self.parse_private_identifier();
            self.expect(Kind::In)?;
            let right = self.parse_unary_expression_or_higher(lhs_span)?;
            Expression::PrivateInExpression(self.ast.alloc(PrivateInExpression {
                span: self.end_span(lhs_span),
                left,
                operator: BinaryOperator::In,
                right,
            }))
        } else {
            self.parse_unary_expression_or_higher(lhs_span)?
        };

        self.parse_binary_expression_rest(lhs_span, lhs, lhs_precedence)
    }

    /// Section 13.6 - 13.13 Binary Expression
    fn parse_binary_expression_rest(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
        min_precedence: Precedence,
    ) -> Result<Expression<'a>> {
        // Pratt Parsing Algorithm
        // <https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html>
        let mut lhs = lhs;
        loop {
            // re-lex for `>=` `>>` `>>>`
            // This is need for jsx `<div>=</div>` case
            let kind = self.re_lex_right_angle();

            let Some(left_precedence) = kind_to_precedence(kind) else { break };

            let stop = if left_precedence.is_right_associative() {
                left_precedence < min_precedence
            } else {
                left_precedence <= min_precedence
            };

            if stop {
                break;
            }

            // Omit the In keyword for the grammar in 13.10 Relational Operators
            // RelationalExpression[In, Yield, Await] :
            // [+In] RelationalExpression[+In, ?Yield, ?Await] in ShiftExpression[?Yield, ?Await]
            if kind == Kind::In && !self.ctx.has_in() {
                break;
            }

            if self.ts_enabled() && matches!(kind, Kind::As | Kind::Satisfies) {
                if self.cur_token().is_on_new_line {
                    break;
                }
                self.bump_any();
                let type_annotation = self.parse_ts_type()?;
                let span = self.end_span(lhs_span);
                lhs = if kind == Kind::As {
                    self.ast.ts_as_expression(span, lhs, type_annotation)
                } else {
                    self.ast.ts_satisfies_expression(span, lhs, type_annotation)
                };
                continue;
            }

            self.bump_any(); // bump operator
            let rhs = self.parse_binary_expression_or_higher(left_precedence)?;

            lhs = if kind.is_logical_operator() {
                self.ast.logical_expression(
                    self.end_span(lhs_span),
                    lhs,
                    map_logical_operator(kind),
                    rhs,
                )
            } else if kind.is_binary_operator() {
                self.ast.binary_expression(
                    self.end_span(lhs_span),
                    lhs,
                    map_binary_operator(kind),
                    rhs,
                )
            } else {
                break;
            };
        }

        Ok(lhs)
    }

    /// Section 13.14 Conditional Expression
    /// `ConditionalExpression`[In, Yield, Await] :
    ///     `ShortCircuitExpression`[?In, ?Yield, ?Await]
    ///     `ShortCircuitExpression`[?In, ?Yield, ?Await] ? `AssignmentExpression`[+In, ?Yield, ?Await] : `AssignmentExpression`[?In, ?Yield, ?Await]
    fn parse_conditional_expression_rest(
        &mut self,
        lhs_span: Span,
        lhs: Expression<'a>,
    ) -> Result<Expression<'a>> {
        if !self.eat(Kind::Question) {
            return Ok(lhs);
        }
        let consequent = self.context(
            Context::In,
            Context::empty(),
            Self::parse_assignment_expression_or_higher,
        )?;
        self.expect(Kind::Colon)?;
        let alternate = self.parse_assignment_expression_or_higher()?;
        Ok(self.ast.conditional_expression(self.end_span(lhs_span), lhs, consequent, alternate))
    }

    /// `AssignmentExpression`[In, Yield, Await] :
    pub(crate) fn parse_assignment_expression_or_higher(&mut self) -> Result<Expression<'a>> {
        // [+Yield] YieldExpression
        if self.is_yield_expression() {
            return self.parse_yield_expression();
        }
        // `(x) => {}`
        if let Some(arrow_expr) = self.try_parse_parenthesized_arrow_function_expression()? {
            return Ok(arrow_expr);
        }
        // `async x => {}`
        if let Some(arrow_expr) = self.try_parse_async_simple_arrow_function_expression()? {
            return Ok(arrow_expr);
        }

        let span = self.start_span();
        let lhs = self.parse_binary_expression_or_higher(Precedence::lowest())?;
        let kind = self.cur_kind();

        // `x => {}`
        if lhs.is_identifier_reference() && kind == Kind::Arrow {
            return self.parse_simple_arrow_function_expression(span, lhs, /* async */ false);
        }

        if kind.is_assignment_operator() {
            return self.parse_assignment_expression_recursive(span, lhs);
        }

        self.parse_conditional_expression_rest(span, lhs)
    }

    fn parse_assignment_expression_recursive(
        &mut self,
        span: Span,
        lhs: Expression<'a>,
    ) -> Result<Expression<'a>> {
        let operator = map_assignment_operator(self.cur_kind());
        // 13.15.5 Destructuring Assignment
        // LeftHandSideExpression = AssignmentExpression
        // is converted to
        // AssignmentPattern[Yield, Await] :
        //    ObjectAssignmentPattern
        //    ArrayAssignmentPattern
        let left = AssignmentTarget::cover(lhs, self)?;
        self.bump_any();
        let right = self.parse_assignment_expression_or_higher()?;
        Ok(self.ast.assignment_expression(self.end_span(span), operator, left, right))
    }

    /// Section 13.16 Sequence Expression
    fn parse_sequence_expression(
        &mut self,
        span: Span,
        first_expression: Expression<'a>,
    ) -> Result<Expression<'a>> {
        let mut expressions = self.ast.new_vec_single(first_expression);
        while self.eat(Kind::Comma) {
            let expression = self.parse_assignment_expression_or_higher()?;
            expressions.push(expression);
        }
        Ok(self.ast.sequence_expression(self.end_span(span), expressions))
    }

    /// ``AwaitExpression`[Yield]` :
    ///     await `UnaryExpression`[?Yield, +Await]
    fn parse_await_expression(&mut self, lhs_span: Span) -> Result<Expression<'a>> {
        let span = self.start_span();
        self.bump_any();
        let has_await = self.ctx.has_await();
        if !has_await {
            self.error(diagnostics::await_expression(Span::new(span.start, span.start + 5)));
        }
        let argument = self.context(Context::Await, Context::empty(), |p| {
            p.parse_simple_unary_expression(lhs_span)
        })?;
        Ok(self.ast.await_expression(self.end_span(span), argument))
    }

    /// `Decorator`[Yield, Await]:
    ///   `DecoratorMemberExpression`[?Yield, ?Await]
    ///   ( `Expression`[+In, ?Yield, ?Await] )
    ///   `DecoratorCallExpression`
    pub(crate) fn parse_decorator(&mut self) -> Result<Decorator<'a>> {
        let span = self.start_span();
        self.bump_any(); // bump @
        let expr = self.context(
            Context::Decorator,
            Context::empty(),
            Self::parse_lhs_expression_or_higher,
        )?;
        Ok(self.ast.decorator(self.end_span(span), expr))
    }

    fn is_update_expression(&self) -> bool {
        match self.cur_kind() {
            kind if kind.is_unary_operator() => false,
            Kind::Await => false,
            Kind::LAngle => {
                if !self.source_type.is_jsx() {
                    return false;
                }
                true
            }
            _ => true,
        }
    }

    fn is_await_expression(&mut self) -> bool {
        if self.at(Kind::Await) {
            let peek_token = self.peek_token();
            // Allow arrow expression `await => {}`
            if peek_token.kind == Kind::Arrow {
                return false;
            }
            if self.ctx.has_await() {
                return true;
            }
            // The following expressions are ambiguous
            // await + 0, await - 0, await ( 0 ), await [ 0 ], await / 0 /u, await ``, await of []
            if matches!(
                peek_token.kind,
                Kind::Of | Kind::LParen | Kind::LBrack | Kind::Slash | Kind::RegExp
            ) {
                return false;
            }

            return !peek_token.is_on_new_line && peek_token.kind.is_after_await_or_yield();
        }
        false
    }

    fn is_yield_expression(&mut self) -> bool {
        if self.at(Kind::Yield) {
            let peek_token = self.peek_token();
            // Allow arrow expression `yield => {}`
            if peek_token.kind == Kind::Arrow {
                return false;
            }
            if self.ctx.has_yield() {
                return true;
            }
            return !peek_token.is_on_new_line && peek_token.kind.is_after_await_or_yield();
        }
        false
    }
}
