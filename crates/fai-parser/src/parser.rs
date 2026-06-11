use crate::ast::*;
use crate::lexer::Lexer;
use crate::token::{Token, TokenType};

pub struct Parser {
    tokens: Vec<Token>,
    index: usize,
    errors: Vec<String>,
    /// Type parameters currently in scope (from <T, U> on functions/types).
    type_params_in_scope: Vec<String>,
    /// Accumulated doc comment lines from `#` comments before a declaration.
    pending_doc: Vec<String>,
    /// Nesting depth inside `[...]` array literals. When > 0, `-` is treated
    /// as unary (start of a new negative item) rather than binary subtraction.
    array_depth: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            index: 0,
            errors: Vec::new(),
            type_params_in_scope: Vec::new(),
            pending_doc: Vec::new(),
            array_depth: 0,
        }
    }

    pub fn parse_program(&mut self) -> Result<Program, String> {
        let mut statements = Vec::new();
        let mut private_mode = false;

        self.skip_newlines();
        // If the leading comments aren't followed by a declaration
        // that accepts a doc comment (`def`, `remote def`, etc.),
        // they aren't doc comments — move them to
        // Program.leading_comments so the formatter can emit them
        // verbatim. Doc comments on the first `def` still flow through
        // `pending_doc` unchanged.
        let first_is_doc_target = self.check(TokenType::Def) || self.check(TokenType::Remote);
        let leading_comments =
            if !self.pending_doc.is_empty() && !first_is_doc_target && !self.is_at_end() {
                std::mem::take(&mut self.pending_doc)
            } else {
                Vec::new()
            };
        while !self.is_at_end() {
            if self.match_t(TokenType::Private) {
                self.consume(TokenType::Colon, "Expected ':' after private")?;
                self.consume(TokenType::Newline, "Expected newline after private:")?;
                private_mode = true;
                self.skip_newlines();
                continue;
            }

            let mut stmt = self.parse_statement()?;
            if private_mode {
                match &mut stmt {
                    Statement::Let(s) => s.is_private = true,
                    Statement::Var(s) => s.is_private = true,
                    Statement::Function(s) => s.is_private = true,
                    Statement::Type(s) => s.is_private = true,
                    Statement::Enum(s) => s.is_private = true,
                    Statement::ExternBlock(s) => s.is_private = true,
                    _ => {}
                }
            }
            statements.push(stmt);
            self.skip_newlines();
        }

        if !self.errors.is_empty() {
            return Err(self.errors.join("\n"));
        }

        Ok(Program {
            statements,
            leading_comments,
        })
    }

    pub fn parse_standalone_expression(&mut self) -> Result<Expression, String> {
        let expr = self.parse_expression()?;
        self.skip_newlines();
        if !self.is_at_end() {
            return Err(format!(
                "Unexpected tokens after expression at {}:{}",
                self.peek().line,
                self.peek().column
            ));
        }
        Ok(expr)
    }

    fn parse_statement(&mut self) -> Result<Statement, String> {
        // `shared let …` / `shared var …` — `shared` is a CONTEXTUAL keyword
        // (still usable as an identifier elsewhere): only special immediately
        // before `let`/`var`. Marks the binding as deliberately aliased.
        if self.check(TokenType::Identifier)
            && self.peek().lexeme == "shared"
            && matches!(
                self.peek_next().token_type,
                TokenType::Let | TokenType::Var
            )
        {
            self.advance(); // consume `shared`
            let mutable = self.match_t(TokenType::Var);
            if !mutable {
                self.advance(); // consume `let`
            }
            return self.parse_variable_statement(mutable, true);
        }
        if self.match_t(TokenType::Let) {
            return self.parse_variable_statement(false, false);
        }
        if self.match_t(TokenType::Var) {
            return self.parse_variable_statement(true, false);
        }
        if self.match_t(TokenType::Use) {
            return self.parse_use_statement();
        }
        if self.match_t(TokenType::Def) {
            return self.parse_function_declaration(false);
        }
        if self.match_t(TokenType::Remote) {
            // `remote def` = remote function, `remote type` = remote type
            if self.match_t(TokenType::Def) {
                return self.parse_function_declaration(true);
            }
            if self.match_t(TokenType::Type) {
                return self.parse_type_declaration_with_remote(true);
            }
            return Err("Expected 'def' or 'type' after 'remote'".to_string());
        }
        if self.match_t(TokenType::Type) {
            // `type def` = function type definition, `type Name` = struct type
            if self.check(TokenType::Def) {
                return self.parse_function_type_def();
            }
            return self.parse_type_declaration();
        }
        if self.match_t(TokenType::Enum) {
            return self.parse_enum_declaration();
        }
        if self.match_t(TokenType::Extern) {
            return self.parse_extern_block();
        }
        // Only parse `test` as a declaration if NOT followed by `.` (member access)
        if self.check(TokenType::Test) && self.peek_next().token_type != TokenType::Dot {
            self.advance();
            return self.parse_test_declaration();
        }
        if self.match_t(TokenType::If) {
            return self.parse_if_statement();
        }
        if self.match_t(TokenType::Case) {
            return self.parse_case_statement();
        }
        if self.match_t(TokenType::Try) {
            return self.parse_try_statement();
        }
        if self.match_t(TokenType::Throw) {
            return self.parse_throw_statement();
        }
        if self.match_t(TokenType::Nowait) {
            return self.parse_nowait_statement();
        }
        if self.match_t(TokenType::For) {
            return self.parse_for_statement();
        }
        if self.match_t(TokenType::While) {
            return self.parse_while_statement();
        }
        if self.match_t(TokenType::Break) {
            let loc = self.location_of_prev();
            return Ok(Statement::Break(loc));
        }
        if self.match_t(TokenType::Continue) {
            let loc = self.location_of_prev();
            return Ok(Statement::Continue(loc));
        }
        if self.match_t(TokenType::Return) {
            let loc = self.location_of_prev();
            // `return` with no expression on the same line → bare
            // return (Void). Otherwise parse a trailing expression.
            let value = if self.check(TokenType::Newline) || self.is_at_end() {
                None
            } else {
                Some(self.parse_value_expression()?)
            };
            return Ok(Statement::Return(ReturnStatement {
                value,
                location: loc,
            }));
        }
        if self.is_assignment_start() {
            return self.parse_assignment_statement();
        }

        let expr = self.parse_value_expression()?;
        let loc = expr.location().clone();
        Ok(Statement::Expression(ExpressionStatement {
            expression: expr,
            location: loc,
        }))
    }

    fn parse_use_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        // use remote <name>
        let is_remote = self.match_t(TokenType::Remote);
        if self.match_t(TokenType::Star) {
            self.consume(TokenType::From, "Expected 'from' after '*' in glob import")?;
            let path = self.parse_module_path()?;
            return Ok(Statement::Use(UseStatement {
                module_path: path,
                imported_names: None,
                import_all: true,
                is_remote,
                location: loc,
            }));
        }
        if self.match_t(TokenType::LeftBrace) {
            let mut names = Vec::new();
            loop {
                names.push(
                    self.consume(TokenType::Identifier, "Expected imported name")?
                        .lexeme
                        .clone(),
                );
                if !self.match_t(TokenType::Comma) {
                    break;
                }
            }
            self.consume(TokenType::RightBrace, "Expected '}'")?;
            self.consume(TokenType::From, "Expected 'from'")?;
            let path = self.parse_module_path()?;
            return Ok(Statement::Use(UseStatement {
                module_path: path,
                imported_names: Some(names),
                import_all: false,
                is_remote,
                location: loc,
            }));
        }
        let path = self.parse_module_path()?;
        Ok(Statement::Use(UseStatement {
            module_path: path,
            imported_names: None,
            import_all: false,
            is_remote,
            location: loc,
        }))
    }

    fn parse_module_path(&mut self) -> Result<Vec<String>, String> {
        let mut path = vec![self.consume_module_segment("Expected module name")?];
        while self.match_t(TokenType::Dot) {
            path.push(self.consume_module_segment("Expected module name segment")?);
        }
        Ok(path)
    }

    /// Consume an identifier or keyword as a module path segment.
    /// Keywords like `test`, `type`, `error` are valid module names in `use std.test`.
    fn consume_module_segment(&mut self, msg: &str) -> Result<String, String> {
        if self.check(TokenType::Identifier) {
            return Ok(self.advance().lexeme.clone());
        }
        // Allow keywords as module path segments
        if self.peek().token_type.is_keyword() {
            return Ok(self.advance().lexeme.clone());
        }
        Err(format!(
            "{} at {}:{}, found {:?} '{}'",
            msg,
            self.peek().line,
            self.peek().column,
            self.peek().token_type,
            self.peek().lexeme
        ))
    }

    fn parse_variable_statement(
        &mut self,
        mutable: bool,
        is_shared: bool,
    ) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let mut bindings = vec![self.parse_binding_declaration()?];
        while self.match_t(TokenType::Comma) {
            bindings.push(self.parse_binding_declaration()?);
        }
        self.consume(TokenType::Assign, "Expected '=' after variable declaration")?;
        let value = self.parse_value_expression()?;
        if mutable {
            Ok(Statement::Var(VarStatement {
                bindings,
                value,
                is_private: false,
                is_shared,
                location: loc,
            }))
        } else {
            Ok(Statement::Let(LetStatement {
                bindings,
                value,
                is_private: false,
                is_shared,
                location: loc,
            }))
        }
    }

    fn parse_binding_declaration(&mut self) -> Result<BindingDeclaration, String> {
        let name = self
            .consume(TokenType::Identifier, "Expected variable name")?
            .lexeme
            .clone();
        let type_name = if self.check(TokenType::Identifier)
            || self.check(TokenType::Dollar)
            || self.check(TokenType::LeftParen)
        {
            Some(self.parse_type_node()?)
        } else {
            None
        };
        Ok(BindingDeclaration { name, type_name })
    }

    fn parse_function_declaration(&mut self, is_remote: bool) -> Result<Statement, String> {
        let doc_comment = self.take_pending_doc();
        let loc = self.location_of_prev();
        let name = self
            .consume(TokenType::Identifier, "Expected function name")?
            .lexeme
            .clone();
        self.parse_function_declaration_v2(name, doc_comment, loc, is_remote)
    }

    /// Parse new-style function: def name \n @type/@param/@return \n do \n body \n end
    fn parse_function_declaration_v2(
        &mut self,
        name: String,
        doc_comment: Option<String>,
        loc: SourceLocation,
        is_remote: bool,
    ) -> Result<Statement, String> {
        let old_type_params = self.type_params_in_scope.clone();
        self.skip_newlines(); // skip newlines + collect comments between def and first @

        let mut type_params = Vec::new();
        let mut params = Vec::new();
        let mut return_types = Vec::new();
        let mut seen_param = false;
        let mut seen_return = false;

        // Parse @type, @param, @return annotations until we hit `do`
        while self.check(TokenType::At) {
            let annotation_doc = self.take_pending_doc();
            self.advance(); // consume @
                            // After @, the keyword can be an Identifier or a keyword token (type, default)
            let keyword = if self.check(TokenType::Type) || self.check(TokenType::Return) {
                // `type` and `return` are keywords elsewhere; accept
                // them as `@type` / `@return` annotations here.
                self.advance()
            } else {
                self.consume(
                    TokenType::Identifier,
                    "Expected 'type', 'param', or 'return' after '@'",
                )?
            };

            match keyword.lexeme.as_str() {
                "type" => {
                    if seen_param {
                        return Err(format!(
                            "@type must come before @param at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    if seen_return {
                        return Err(format!(
                            "@type must come before @return at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    let tp_loc = self.peek_location();
                    let tp_name = self
                        .consume(
                            TokenType::Identifier,
                            "Expected type parameter name after '@type'",
                        )?
                        .lexeme
                        .clone();
                    self.type_params_in_scope.push(tp_name.clone());
                    type_params.push(TypeParamDeclaration {
                        name: tp_name,
                        doc_comment: annotation_doc,
                        location: tp_loc,
                    });
                }
                "param" => {
                    if seen_return {
                        return Err(format!(
                            "@param must come before @return at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    seen_param = true;
                    let p_loc = self.peek_location();
                    let p_name = self
                        .consume(
                            TokenType::Identifier,
                            "Expected parameter name after '@param'",
                        )?
                        .lexeme
                        .clone();
                    let type_node = self.parse_type_node()?;
                    // Optional comma-separated modifiers: mutable, default: <expr>
                    let mut default_value = None;
                    let mut is_mutable = false;
                    while self.match_t(TokenType::Comma) {
                        let kw = if self.check(TokenType::Default) {
                            self.advance()
                        } else {
                            self.consume(
                                TokenType::Identifier,
                                "Expected 'default' or 'mutable' after ','",
                            )?
                        };
                        match kw.lexeme.as_str() {
                            "default" => {
                                self.consume(TokenType::Colon, "Expected ':' after 'default'")?;
                                default_value = Some(self.parse_expression()?);
                            }
                            "mutable" => {
                                is_mutable = true;
                            }
                            other => {
                                return Err(format!(
                                    "Unknown param modifier '{}' at {}:{}",
                                    other, kw.line, kw.column
                                ));
                            }
                        }
                    }
                    params.push(Parameter {
                        name: p_name,
                        type_node,
                        default_value,
                        is_out: false,
                        is_mutable,
                        location: p_loc,
                        doc_comment: annotation_doc,
                    });
                }
                "return" => {
                    seen_return = true;
                    let r_loc = self.peek_location();
                    // @return Type  OR  @return name Type
                    // Try to parse: if next two tokens are Identifier Identifier (or Identifier LeftParen etc),
                    // the first is the name and the second starts the type.
                    // If it's just a type (e.g., @return Void, @return Int), there's no name.
                    let first_tok = self.peek().clone();
                    let type_node;
                    let ret_name;
                    // Peek ahead: if after first identifier there's another identifier, `(`, `[`, or `?`
                    // then first is the name and we parse the type next.
                    let next_tt = self.peek_next().token_type;
                    if first_tok.token_type == TokenType::Identifier
                        && (next_tt == TokenType::Identifier
                            || next_tt == TokenType::LeftParen
                            || next_tt == TokenType::Less)
                    {
                        // Named return: @return name Type
                        ret_name = Some(self.advance().lexeme.clone());
                        type_node = self.parse_type_node()?;
                    } else {
                        // Unnamed return: @return Type
                        ret_name = None;
                        type_node = self.parse_type_node()?;
                    }
                    return_types.push(ReturnDeclaration {
                        name: ret_name,
                        type_node,
                        doc_comment: annotation_doc,
                        location: r_loc,
                    });
                }
                other => {
                    return Err(format!(
                        "Expected 'type', 'param', or 'return' after '@', got '{}' at {}:{}",
                        other, keyword.line, keyword.column
                    ));
                }
            }
            self.skip_newlines();
        }

        // If no @return was specified, default to Void
        if return_types.is_empty() {
            return_types.push(ReturnDeclaration {
                name: None,
                type_node: TypeNode {
                    name: Some("Void".into()),
                    is_type_parameter: false,
                    function_params: None,
                    function_returns: None,
                    is_array: false,
                    is_optional: false,
                    location: self.peek_location(),
                },
                doc_comment: None,
                location: self.peek_location(),
            });
        }

        // If `do` is present, parse the body. Otherwise this is an
        // abstract function declaration (interface/signature only).
        let (body, is_abstract) = if self.check(TokenType::Do) {
            self.advance(); // consume 'do'
            self.consume(TokenType::Newline, "Expected newline after 'do'")?;
            let body = self.parse_block_until(&[TokenType::End])?;
            self.consume(TokenType::End, "Expected 'end' after function body")?;
            (body, false)
        } else {
            (Vec::new(), true)
        };
        self.type_params_in_scope = old_type_params;

        // Clear any pending doc that wasn't consumed
        self.pending_doc.clear();

        Ok(Statement::Function(FunctionDeclaration {
            name,
            type_params,
            params,
            return_types,
            body,
            is_private: false,
            is_abstract,
            is_remote,
            location: loc,
            doc_comment,
        }))
    }

    /// Parse a `do...end` block as an anonymous closure.
    /// Syntax: `do [with name Type, name Type] ... end`
    fn parse_do_block(&mut self) -> Result<Expression, String> {
        self.advance(); // consume 'do'
        let loc = self.location_of_prev();

        // Parse optional `with` parameters: `do with x Int, y String`
        let mut params = Vec::new();
        if self.match_t(TokenType::With) {
            loop {
                let ploc = self.peek_location();
                let name = self
                    .consume(
                        TokenType::Identifier,
                        "Expected parameter name after 'with'",
                    )?
                    .lexeme
                    .clone();
                let type_node = self.parse_type_node()?;
                params.push(Parameter {
                    name,
                    type_node,
                    default_value: None,
                    is_out: false,
                    is_mutable: false,
                    location: ploc,
                    doc_comment: None,
                });
                if !self.match_t(TokenType::Comma) {
                    break;
                }
            }
        }

        // Return type left empty -- the checker infers it from context/body
        let return_types = Vec::new();

        self.skip_newlines();
        let body = self.parse_block_until(&[TokenType::End])?;
        self.consume(TokenType::End, "Expected 'end' to close 'do' block")?;

        // Generate a synthetic name
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("<block:{}>", id);

        Ok(Expression::Function(FunctionDeclaration {
            name,
            type_params: Vec::new(),
            params,
            return_types,
            body,
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: loc,
            doc_comment: None,
        }))
    }

    fn parse_anonymous_function(&mut self) -> Result<Expression, String> {
        self.advance(); // consume 'def'
        let loc = self.location_of_prev();
        self.consume(TokenType::LeftParen, "Expected '(' after 'def'")?;
        let mut params = Vec::new();
        if !self.check(TokenType::RightParen) {
            loop {
                params.push(self.parse_parameter()?);
                if !self.match_t(TokenType::Comma) {
                    break;
                }
            }
        }
        self.consume(TokenType::RightParen, "Expected ')'")?;
        let mut return_types = Vec::new();
        if self.match_t(TokenType::Arrow) {
            let rloc = self.peek_location();
            let tn = self.parse_type_node()?;
            return_types.push(ReturnDeclaration {
                name: None,
                type_node: tn,
                doc_comment: None,
                location: rloc,
            });
        } else {
            return_types.push(ReturnDeclaration {
                name: None,
                type_node: TypeNode {
                    name: Some("Void".into()),
                    is_type_parameter: false,
                    function_params: None,
                    function_returns: None,
                    is_array: false,
                    is_optional: false,
                    location: self.peek_location(),
                },
                doc_comment: None,
                location: self.peek_location(),
            });
        }
        self.consume(
            TokenType::Newline,
            "Expected newline after function signature",
        )?;
        let body = self.parse_block_until(&[TokenType::End])?;
        self.consume(TokenType::End, "Expected 'end'")?;
        // Generate a synthetic name for the anonymous function
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("<anon:{}>", id);
        Ok(Expression::Function(FunctionDeclaration {
            name,
            type_params: Vec::new(),
            params,
            return_types,
            body,
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: loc,
            doc_comment: None,
        }))
    }

    fn parse_parameter(&mut self) -> Result<Parameter, String> {
        let loc = self.peek_location();
        let name = self
            .consume(TokenType::Identifier, "Expected parameter name")?
            .lexeme
            .clone();
        self.consume(TokenType::Colon, "Expected ':' after parameter name")?;
        let type_node = self.parse_type_node()?;
        let default_value = if self.match_t(TokenType::Assign) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(Parameter {
            name,
            type_node,
            default_value,
            is_out: false,
            is_mutable: false,
            location: loc,
            doc_comment: None,
        })
    }

    fn parse_type_declaration(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let name = self
            .consume(TokenType::Identifier, "Expected type name")?
            .lexeme
            .clone();
        let old_type_params = self.type_params_in_scope.clone();
        self.consume(TokenType::Newline, "Expected newline after type name")?;
        let mut type_params = Vec::new();
        let mut fields = Vec::new();
        self.skip_newlines();
        // Parse @type lines before fields
        while self.check(TokenType::At) {
            let _doc = self.take_pending_doc();
            self.advance(); // consume @
            let kw = if self.check(TokenType::Type) {
                self.advance()
            } else {
                self.consume(
                    TokenType::Identifier,
                    "Expected 'type' after '@' in type declaration",
                )?
            };
            if kw.lexeme != "type" {
                return Err(format!(
                    "Expected '@type' in type declaration, got '@{}' at {}:{}",
                    kw.lexeme, kw.line, kw.column
                ));
            }
            let tp_loc = self.peek_location();
            let tp_name = self
                .consume(
                    TokenType::Identifier,
                    "Expected type parameter name after '@type'",
                )?
                .lexeme
                .clone();
            self.type_params_in_scope.push(tp_name.clone());
            type_params.push(TypeParamDeclaration {
                name: tp_name,
                doc_comment: _doc,
                location: tp_loc,
            });
            self.skip_newlines();
        }
        // Parse fields
        while !self.check(TokenType::End) {
            let field_name = self
                .consume(TokenType::Identifier, "Expected field name")?
                .lexeme
                .clone();
            let type_node = self.parse_type_node()?;
            let default_value = if self.match_t(TokenType::Assign) {
                Some(self.parse_expression()?)
            } else {
                None
            };
            // Optional comma-separated attributes: alias: "key", omit, ...
            let mut attributes = Vec::new();
            while self.match_t(TokenType::Comma) {
                let key_tok =
                    self.consume(TokenType::Identifier, "Expected attribute name after ','")?;
                let key = key_tok.lexeme.clone();
                if self.match_t(TokenType::Colon) {
                    if self.check(TokenType::String) {
                        let s = self.advance().lexeme.clone();
                        attributes.push(FieldAttribute {
                            key,
                            value: FieldAttributeValue::String(s),
                        });
                    } else {
                        return Err(format!(
                            "Expected string literal for attribute '{}' at {}:{}",
                            key, key_tok.line, key_tok.column
                        ));
                    }
                } else {
                    attributes.push(FieldAttribute {
                        key,
                        value: FieldAttributeValue::Flag,
                    });
                }
            }
            let floc = self.location_of_prev();
            fields.push(FieldDeclaration {
                name: field_name,
                type_node,
                default_value,
                attributes,
                location: floc,
            });
            self.skip_newlines();
        }
        self.consume(TokenType::End, "Expected 'end' after type declaration")?;
        self.type_params_in_scope = old_type_params;
        self.pending_doc.clear();
        Ok(Statement::Type(TypeDeclaration {
            name,
            type_params,
            fields,
            is_private: false,
            is_remote: false,
            location: loc,
        }))
    }

    /// Parse `remote type Name ... end` — a type that's part of the RPC interface.
    fn parse_type_declaration_with_remote(&mut self, is_remote: bool) -> Result<Statement, String> {
        // Reuse the normal type parser but set is_remote on the result
        match self.parse_type_declaration()? {
            Statement::Type(mut td) => {
                td.is_remote = is_remote;
                Ok(Statement::Type(td))
            }
            other => Ok(other),
        }
    }

    /// Parse `type def MyHandler @param ... @return ... end`
    fn parse_function_type_def(&mut self) -> Result<Statement, String> {
        let doc_comment = self.take_pending_doc();
        self.advance(); // consume 'def'
        let loc = self.location_of_prev();
        let name = self
            .consume(
                TokenType::Identifier,
                "Expected function type name after 'type def'",
            )?
            .lexeme
            .clone();
        self.skip_newlines();

        let mut type_params = Vec::new();
        let mut params = Vec::new();
        let mut return_types = Vec::new();
        let mut seen_param = false;
        let mut seen_return = false;

        while self.check(TokenType::At) {
            let annotation_doc = self.take_pending_doc();
            self.advance(); // consume @
            let keyword = if self.check(TokenType::Type) || self.check(TokenType::Return) {
                // `type` and `return` are keywords elsewhere; accept
                // them as `@type` / `@return` annotations here.
                self.advance()
            } else {
                self.consume(
                    TokenType::Identifier,
                    "Expected 'type', 'param', or 'return' after '@'",
                )?
            };

            match keyword.lexeme.as_str() {
                "type" => {
                    if seen_param {
                        return Err(format!(
                            "@type must come before @param at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    if seen_return {
                        return Err(format!(
                            "@type must come before @return at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    let tp_loc = self.peek_location();
                    let tp_name = self
                        .consume(TokenType::Identifier, "Expected type parameter name")?
                        .lexeme
                        .clone();
                    type_params.push(TypeParamDeclaration {
                        name: tp_name,
                        doc_comment: annotation_doc,
                        location: tp_loc,
                    });
                }
                "param" => {
                    if seen_return {
                        return Err(format!(
                            "@param must come before @return at {}:{}",
                            keyword.line, keyword.column
                        ));
                    }
                    seen_param = true;
                    let p_loc = self.peek_location();
                    let p_name = self
                        .consume(TokenType::Identifier, "Expected parameter name")?
                        .lexeme
                        .clone();
                    let type_node = self.parse_type_node()?;
                    params.push(Parameter {
                        name: p_name,
                        type_node,
                        default_value: None,
                        is_out: false,
                        is_mutable: false,
                        location: p_loc,
                        doc_comment: annotation_doc,
                    });
                }
                "return" => {
                    seen_return = true;
                    let r_loc = self.peek_location();
                    let type_node = self.parse_type_node()?;
                    return_types.push(ReturnDeclaration {
                        name: None,
                        type_node,
                        doc_comment: annotation_doc,
                        location: r_loc,
                    });
                }
                other => {
                    return Err(format!(
                        "Expected 'type', 'param', or 'return' after '@', got '{}' at {}:{}",
                        other, keyword.line, keyword.column
                    ));
                }
            }
            self.skip_newlines();
        }

        if return_types.is_empty() {
            return_types.push(ReturnDeclaration {
                name: None,
                type_node: TypeNode {
                    name: Some("Void".into()),
                    is_type_parameter: false,
                    function_params: None,
                    function_returns: None,
                    is_array: false,
                    is_optional: false,
                    location: self.peek_location(),
                },
                doc_comment: None,
                location: self.peek_location(),
            });
        }

        self.consume(TokenType::End, "Expected 'end' after type def")?;
        self.pending_doc.clear();

        Ok(Statement::FunctionTypeDef(FunctionTypeDefDeclaration {
            name,
            type_params,
            params,
            return_types,
            is_private: false,
            doc_comment,
            location: loc,
        }))
    }

    fn parse_enum_declaration(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let name = self
            .consume(TokenType::Identifier, "Expected enum name")?
            .lexeme
            .clone();
        self.consume(TokenType::Newline, "Expected newline after enum name")?;
        let mut members = Vec::new();
        self.skip_newlines();
        while !self.check(TokenType::End) {
            members.push(
                self.consume(TokenType::Identifier, "Expected enum member")?
                    .lexeme
                    .clone(),
            );
            self.skip_newlines();
        }
        self.consume(TokenType::End, "Expected 'end' after enum declaration")?;
        Ok(Statement::Enum(EnumDeclaration {
            name,
            members,
            is_private: false,
            location: loc,
        }))
    }

    fn parse_extern_block(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let library = self
            .consume(
                TokenType::Identifier,
                "Expected library name after 'extern'",
            )?
            .lexeme
            .clone();
        self.consume(
            TokenType::Newline,
            "Expected newline after extern library name",
        )?;
        self.skip_newlines();

        let mut types = Vec::new();
        let mut functions = Vec::new();

        while !self.check(TokenType::End) {
            if self.match_t(TokenType::Type) {
                // Opaque type declaration: `type Db`
                let type_loc = self.location_of_prev();
                let name = self
                    .consume(TokenType::Identifier, "Expected type name")?
                    .lexeme
                    .clone();
                types.push(ExternTypeDecl {
                    name,
                    location: type_loc,
                });
            } else if self.match_t(TokenType::Def) {
                // Extern function declaration: `def open(path: String) -> Db`
                let fn_loc = self.location_of_prev();
                let name = self
                    .consume(TokenType::Identifier, "Expected function name")?
                    .lexeme
                    .clone();
                self.consume(TokenType::LeftParen, "Expected '(' after function name")?;
                let mut params = Vec::new();
                if !self.check(TokenType::RightParen) {
                    loop {
                        // Check for `out` keyword before param name
                        let is_out =
                            self.check(TokenType::Identifier) && self.peek().lexeme == "out";
                        if is_out {
                            self.advance();
                        }
                        let mut p = self.parse_parameter()?;
                        p.is_out = is_out;
                        params.push(p);
                        if !self.match_t(TokenType::Comma) {
                            break;
                        }
                    }
                }
                self.consume(TokenType::RightParen, "Expected ')'")?;
                let return_type = if self.match_t(TokenType::Arrow) {
                    Some(self.parse_type_node()?)
                } else {
                    None
                };
                // Optional `variadic N` for variadic C functions
                let fixed_arg_count =
                    if self.check(TokenType::Identifier) && self.peek().lexeme == "variadic" {
                        self.advance();
                        let n_tok =
                            self.consume(TokenType::Number, "Expected number after 'variadic'")?;
                        let n: usize = n_tok.lexeme.parse().map_err(|_| {
                            format!(
                                "Invalid variadic count '{}' at {}:{}",
                                n_tok.lexeme, n_tok.line, n_tok.column
                            )
                        })?;
                        Some(n)
                    } else {
                        None
                    };
                functions.push(ExternFunctionDecl {
                    name,
                    params,
                    return_type,
                    fixed_arg_count,
                    location: fn_loc,
                });
            } else {
                return Err(format!(
                    "Expected 'type' or 'def' in extern block at {}:{}, found {:?} '{}'",
                    self.peek().line,
                    self.peek().column,
                    self.peek().token_type,
                    self.peek().lexeme
                ));
            }
            self.skip_newlines();
        }
        self.consume(TokenType::End, "Expected 'end' after extern block")?;

        Ok(Statement::ExternBlock(ExternBlockDeclaration {
            library,
            types,
            functions,
            is_private: false,
            location: loc,
        }))
    }

    fn parse_test_declaration(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let name = self
            .consume(TokenType::Identifier, "Expected test name")?
            .lexeme
            .clone();
        self.consume(TokenType::Newline, "Expected newline after test name")?;
        let mut setup = Vec::new();
        let mut before_all = None;
        let mut before_each = None;
        let mut after_each = None;
        let mut after_all = None;
        let mut cases = Vec::new();
        self.skip_newlines();
        while !self.check(TokenType::End) && !self.is_at_end() {
            self.skip_newlines();
            if self.check(TokenType::End) || self.is_at_end() {
                break;
            }
            if self.match_t(TokenType::BeforeAll) {
                self.consume(TokenType::Newline, "Expected newline")?;
                before_all = Some(self.parse_block_until(&[TokenType::End])?);
                self.consume(TokenType::End, "Expected 'end'")?;
                continue;
            }
            if self.match_t(TokenType::BeforeEach) {
                self.consume(TokenType::Newline, "Expected newline")?;
                before_each = Some(self.parse_block_until(&[TokenType::End, TokenType::It])?);
                self.consume(TokenType::End, "Expected 'end'")?;
                continue;
            }
            if self.match_t(TokenType::AfterEach) {
                self.consume(TokenType::Newline, "Expected newline")?;
                after_each = Some(self.parse_block_until(&[TokenType::End])?);
                self.consume(TokenType::End, "Expected 'end'")?;
                continue;
            }
            if self.match_t(TokenType::AfterAll) {
                self.consume(TokenType::Newline, "Expected newline")?;
                after_all = Some(self.parse_block_until(&[TokenType::End])?);
                self.consume(TokenType::End, "Expected 'end'")?;
                continue;
            }
            if self.match_t(TokenType::It) {
                let desc_tok = self.consume_any(
                    &[TokenType::String, TokenType::Identifier],
                    "Expected test description",
                )?;
                let desc = desc_tok.lexeme.clone();
                let dloc = self.location_of_prev();
                self.consume(TokenType::Newline, "Expected newline")?;
                let body = self.parse_block_until(&[TokenType::End, TokenType::It])?;
                self.consume(TokenType::End, "Expected 'end'")?;
                cases.push(TestCase {
                    description: desc,
                    body,
                    location: dloc,
                });
                continue;
            }
            setup.push(self.parse_statement()?);
            self.skip_newlines();
        }
        self.consume(TokenType::End, "Expected 'end' after test block")?;
        Ok(Statement::Test(TestDeclaration {
            name,
            setup,
            before_all,
            before_each,
            cases,
            after_each,
            after_all,
            location: loc,
        }))
    }

    fn parse_if_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let cond = self.parse_expression()?;
        // Special case `if cond then ... else ... end` — agents
        // (and humans) reach for this from JS/Rust/Swift. forai's
        // `if` is statement-form and there's no `then` keyword.
        // Catch it explicitly with an actionable error rather than
        // letting the bare "expected newline" message strand them.
        if self.peek().lexeme == "then" {
            return Err(format!(
                "Expected newline after `if` condition at {}:{}, found `then`. \
                 forai's `if` is statement-form (no `then` keyword, no inline value). \
                 For a value that depends on a condition, lift to a `var` you assign \
                 inside a multi-line if:\n\n  \
                 var result = 0\n  \
                 if cond\n      result = a\n  \
                 else\n      result = b\n  \
                 end",
                self.peek().line,
                self.peek().column,
            ));
        }
        self.consume(TokenType::Newline, "Expected newline after if condition")?;
        let cloc = cond.location().clone();
        let body = self.parse_block_until(&[TokenType::Else, TokenType::End])?;
        let mut branches = vec![IfBranch {
            condition: cond,
            body,
            location: cloc,
        }];
        let mut else_branch = None;

        while self.match_t(TokenType::Else) {
            if self.match_t(TokenType::If) {
                let c = self.parse_expression()?;
                self.consume(TokenType::Newline, "Expected newline")?;
                let cl = c.location().clone();
                let b = self.parse_block_until(&[TokenType::Else, TokenType::End])?;
                branches.push(IfBranch {
                    condition: c,
                    body: b,
                    location: cl,
                });
                continue;
            }
            self.consume(TokenType::Newline, "Expected newline after else")?;
            else_branch = Some(self.parse_block_until(&[TokenType::End])?);
            break;
        }
        self.consume(TokenType::End, "Expected 'end' after if statement")?;
        Ok(Statement::If(IfStatement {
            branches,
            else_branch,
            location: loc,
        }))
    }

    fn parse_case_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let value = self.parse_expression()?;
        self.consume(TokenType::Newline, "Expected newline after case value")?;
        let mut when_branches = Vec::new();
        let mut default_branch = None;
        self.skip_newlines();
        while !self.check(TokenType::End) {
            if self.match_t(TokenType::When) {
                let m = self.parse_expression()?;
                self.consume(TokenType::Newline, "Expected newline")?;
                let ml = m.location().clone();
                let b =
                    self.parse_block_until(&[TokenType::When, TokenType::Default, TokenType::End])?;
                when_branches.push(CaseBranch {
                    match_expr: m,
                    body: b,
                    location: ml,
                });
                continue;
            }
            if self.match_t(TokenType::Default) {
                self.consume(TokenType::Newline, "Expected newline")?;
                default_branch = Some(self.parse_block_until(&[TokenType::End])?);
                break;
            }
            return Err(format!(
                "Expected 'when', 'default', or 'end' at {}:{}",
                self.peek().line,
                self.peek().column
            ));
        }
        self.consume(TokenType::End, "Expected 'end' after case")?;
        Ok(Statement::Case(CaseStatement {
            value,
            when_branches,
            default_branch,
            location: loc,
        }))
    }

    fn parse_try_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        self.consume(TokenType::Newline, "Expected newline after try")?;
        let try_body =
            self.parse_block_until(&[TokenType::Catch, TokenType::Finally, TokenType::End])?;
        self.consume(TokenType::Catch, "Expected 'catch'")?;
        let catch_name = self
            .consume(TokenType::Identifier, "Expected catch variable")?
            .lexeme
            .clone();
        self.consume(TokenType::Newline, "Expected newline")?;
        let catch_body = self.parse_block_until(&[TokenType::Finally, TokenType::End])?;
        let finally_body = if self.match_t(TokenType::Finally) {
            self.consume(TokenType::Newline, "Expected newline")?;
            Some(self.parse_block_until(&[TokenType::End])?)
        } else {
            None
        };
        self.consume(TokenType::End, "Expected 'end' after try")?;
        Ok(Statement::Try(TryStatement {
            try_body,
            catch_name,
            catch_body,
            finally_body,
            location: loc,
        }))
    }

    fn parse_throw_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let expr = self.parse_expression()?;
        Ok(Statement::Throw(ThrowStatement {
            expression: expr,
            location: loc,
        }))
    }

    fn parse_nowait_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let expr = self.parse_expression()?;
        Ok(Statement::Nowait(NowaitStatement {
            expression: expr,
            location: loc,
        }))
    }

    fn parse_for_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let item_name = self
            .consume(TokenType::Identifier, "Expected loop variable")?
            .lexeme
            .clone();
        self.consume(TokenType::In, "Expected 'in'")?;
        let items = self.parse_expression()?;
        self.consume(TokenType::Newline, "Expected newline")?;
        let body = self.parse_block_until(&[TokenType::End])?;
        self.consume(TokenType::End, "Expected 'end' after for loop")?;
        Ok(Statement::For(ForStatement {
            item_name,
            items,
            body,
            location: loc,
        }))
    }

    fn parse_while_statement(&mut self) -> Result<Statement, String> {
        let loc = self.location_of_prev();
        let condition = self.parse_expression()?;
        self.consume(TokenType::Newline, "Expected newline after while condition")?;
        let body = self.parse_block_until(&[TokenType::End])?;
        self.consume(TokenType::End, "Expected 'end' after while loop")?;
        Ok(Statement::While(WhileStatement {
            condition,
            body,
            location: loc,
        }))
    }

    fn parse_assignment_statement(&mut self) -> Result<Statement, String> {
        let loc = self.peek_location();
        let first_name = self
            .consume(TokenType::Identifier, "Expected name")?
            .lexeme
            .clone();

        // Check for field access (x.field = expr) or index access (x[i] = expr)
        if self.check(TokenType::Dot) || self.check(TokenType::LeftBracket) {
            // Parse the full left-hand side as an expression (member/index chain)
            let mut expr = Expression::Identifier(IdentifierExpr {
                name: first_name,
                location: loc.clone(),
            });
            loop {
                if self.match_t(TokenType::Dot) {
                    let prop =
                        self.consume(TokenType::Identifier, "Expected field name after '.'")?;
                    expr = Expression::Member(MemberExpr {
                        object: Box::new(expr),
                        property: prop.lexeme.clone(),
                        location: self.location_of_tok(&prop),
                    });
                } else if self.match_t(TokenType::LeftBracket) {
                    let index = self.parse_expression()?;
                    self.consume(TokenType::RightBracket, "Expected ']'")?;
                    let target = AssignmentTarget::Index(Box::new(Expression::Index(IndexExpr {
                        object: Box::new(expr),
                        index: Box::new(index),
                        location: loc.clone(),
                    })));
                    self.consume(TokenType::Assign, "Expected '='")?;
                    let value = self.parse_value_expression()?;
                    return Ok(Statement::Assignment(AssignmentStatement {
                        target,
                        value,
                        location: loc,
                    }));
                } else {
                    break;
                }
            }
            // Must be a field assignment (ended with .field, not [index])
            self.consume(TokenType::Assign, "Expected '='")?;
            let value = self.parse_value_expression()?;
            return Ok(Statement::Assignment(AssignmentStatement {
                target: AssignmentTarget::Field(Box::new(expr)),
                value,
                location: loc,
            }));
        }

        // Multi-variable assignment: x, y = expr
        let mut names = vec![first_name];
        while self.match_t(TokenType::Comma) {
            names.push(
                self.consume(TokenType::Identifier, "Expected name")?
                    .lexeme
                    .clone(),
            );
        }
        self.consume(TokenType::Assign, "Expected '='")?;
        let value = self.parse_value_expression()?;
        Ok(Statement::Assignment(AssignmentStatement {
            target: AssignmentTarget::Variables(names),
            value,
            location: loc,
        }))
    }

    fn parse_block_until(&mut self, stop: &[TokenType]) -> Result<Vec<Statement>, String> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.is_at_end() && !stop.contains(&self.peek().token_type) {
            // `skip_newlines` accumulates comments into `pending_doc`. If
            // the upcoming statement is a declaration it will claim them as
            // its doc comment; otherwise they are an in-body comment that
            // would otherwise be dropped, so emit them as a standalone
            // Comment statement to survive a fmt round-trip.
            if !self.pending_doc.is_empty() && !self.next_is_doc_target() {
                stmts.push(self.take_pending_comment_statement());
            }
            stmts.push(self.parse_statement()?);
            self.skip_newlines();
        }
        // Comments sitting just before the block's closing token belong to
        // this block; flush them so they are not lost.
        if !self.pending_doc.is_empty() {
            stmts.push(self.take_pending_comment_statement());
        }
        Ok(stmts)
    }

    /// True when the next token starts a declaration that consumes the
    /// pending comments as its doc comment (so they must NOT be flushed as
    /// a standalone Comment statement).
    fn next_is_doc_target(&self) -> bool {
        matches!(
            self.peek().token_type,
            TokenType::Def | TokenType::Remote | TokenType::Type | TokenType::Enum
        )
    }

    /// Drain the accumulated pending comments into a Comment statement.
    fn take_pending_comment_statement(&mut self) -> Statement {
        let lines = std::mem::take(&mut self.pending_doc);
        Statement::Comment(CommentStatement {
            lines,
            location: self.peek_location(),
        })
    }

    fn parse_type_node(&mut self) -> Result<TypeNode, String> {
        let loc = self.peek_location();
        let mut name = None;
        let mut is_type_parameter = false;
        let mut function_params = None;
        let mut function_returns = None;

        if self.match_t(TokenType::LeftParen) {
            let mut params = Vec::new();
            if !self.check(TokenType::RightParen) {
                loop {
                    params.push(self.parse_type_node()?);
                    if !self.match_t(TokenType::Comma) {
                        break;
                    }
                }
            }
            self.consume(TokenType::RightParen, "Expected ')'")?;
            self.consume(TokenType::Arrow, "Expected '->'")?;
            let mut rets = vec![self.parse_type_node()?];
            while self.check(TokenType::Comma) && !self.comma_starts_next_parameter() {
                self.advance();
                rets.push(self.parse_type_node()?);
            }
            function_params = Some(params);
            function_returns = Some(rets);
        } else {
            if self.match_t(TokenType::Dollar) {
                is_type_parameter = true;
            }
            name = Some(
                self.consume(TokenType::Identifier, "Expected type name")?
                    .lexeme
                    .clone(),
            );
            // Check if this name is a type parameter from <T, U> declaration
            if !is_type_parameter {
                if let Some(n) = &name {
                    if self.type_params_in_scope.contains(n) {
                        is_type_parameter = true;
                    }
                }
            }
        }

        let is_array = if self.match_t(TokenType::LeftBracket) {
            self.consume(TokenType::RightBracket, "Expected ']'")?;
            true
        } else {
            false
        };

        let is_optional = self.match_t(TokenType::Question);

        Ok(TypeNode {
            name,
            is_type_parameter,
            function_params,
            function_returns,
            is_array,
            is_optional,
            location: loc,
        })
    }

    // ── Expression parsing (precedence climbing) ───────────────────

    fn parse_expression(&mut self) -> Result<Expression, String> {
        self.parse_or()
    }

    fn parse_value_expression(&mut self) -> Result<Expression, String> {
        let first = self.parse_expression()?;
        if !self.match_t(TokenType::Comma) {
            return Ok(first);
        }
        let loc = first.location().clone();
        let mut items = vec![first];
        loop {
            items.push(self.parse_expression()?);
            if !self.match_t(TokenType::Comma) {
                break;
            }
        }
        Ok(Expression::Tuple(TupleExpr {
            items,
            location: loc,
        }))
    }

    fn parse_or(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_and()?;
        while self.match_t(TokenType::Or) {
            let op = self.previous().lexeme.clone();
            let right = self.parse_and()?;
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_equality()?;
        while self.match_t(TokenType::And) {
            let op = self.previous().lexeme.clone();
            let right = self.parse_equality()?;
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_equality(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_range()?;
        while self.match_any(&[
            TokenType::EqualEqual,
            TokenType::BangEqual,
            TokenType::Greater,
            TokenType::GreaterEqual,
            TokenType::Less,
            TokenType::LessEqual,
        ]) {
            let op = self.previous().lexeme.clone();
            let right = self.parse_range()?;
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_range(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_additive()?;
        loop {
            let inclusive = if self.match_t(TokenType::DotDotDot) {
                true
            } else if self.match_t(TokenType::DotDot) {
                false
            } else {
                break;
            };
            let right = self.parse_additive()?;
            let loc = expr.location().clone();
            expr = Expression::Range(RangeExpr {
                start: Box::new(expr),
                end: Box::new(right),
                inclusive,
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_additive(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_multiplicative()?;
        while self.check(TokenType::Plus) || self.check(TokenType::Minus) {
            // Inside array literals, treat `-` as the start of a new item
            // (unary negation) rather than binary subtraction. This makes
            // `[10 -5]` parse as two items `[10, -5]` instead of `[10 - 5]`.
            // Use `(10 - 5)` for explicit subtraction inside arrays.
            if self.array_depth > 0 && self.check(TokenType::Minus) {
                break;
            }
            self.advance();
            let op = self.previous().lexeme.clone();
            let right = self.parse_multiplicative()?;
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_unary()?;
        while self.match_any(&[
            TokenType::Star,
            TokenType::Slash,
            TokenType::SlashSlash,
            TokenType::Percent,
        ]) {
            let op = self.previous().lexeme.clone();
            let right = self.parse_unary()?;
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expression, String> {
        if self.match_any(&[TokenType::Minus, TokenType::Bang, TokenType::Not]) {
            let op = self.previous().lexeme.clone();
            let loc = self.location_of_prev();
            let expr = self.parse_unary()?;
            return Ok(Expression::Unary(UnaryExpr {
                operator: op,
                expression: Box::new(expr),
                location: loc,
            }));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_call()?;
        if self.match_t(TokenType::StarStar) {
            let op = self.previous().lexeme.clone();
            let right = self.parse_unary()?; // right-to-left associativity
            let loc = expr.location().clone();
            expr = Expression::Binary(BinaryExpr {
                left: Box::new(expr),
                operator: op,
                right: Box::new(right),
                location: loc,
            });
        }
        Ok(expr)
    }

    fn parse_call(&mut self) -> Result<Expression, String> {
        let mut expr = self.parse_primary()?;

        // Trailing do...end without parens: `VStack do ... end` → VStack(do...end)
        // Only applies to bare identifiers or member access (not to results of calls/indexes).
        //
        // Falls through to the postfix-chain loop below instead of
        // returning, so common patterns like
        //   `VStack do ... end.padding(12)`
        // keep parsing — the `.padding(12)` chains on the call's result.
        // Without this fall-through, agents (and humans) hit "Expected
        // expression, found Dot" and have to lift the do-block to a
        // local just to chain a UFCS modifier.
        if self.check(TokenType::Do) {
            let is_call_target = matches!(&expr, Expression::Identifier(_) | Expression::Member(_));
            if is_call_target {
                let block = self.parse_do_block()?;
                let bloc = block.location().clone();
                let loc = expr.location().clone();
                expr = Expression::Call(CallExpr {
                    callee: Box::new(expr),
                    args: vec![CallArgument {
                        label: None,
                        value: block,
                        location: bloc,
                    }],
                    location: loc,
                });
                // Fall through to the postfix loop below. Don't return.
            }
        }

        loop {
            if self.match_t(TokenType::Dot) {
                let prop = self
                    .consume(TokenType::Identifier, "Expected property name")?
                    .lexeme
                    .clone();
                let loc = expr.location().clone();
                expr = Expression::Member(MemberExpr {
                    object: Box::new(expr),
                    property: prop,
                    location: loc,
                });
                continue;
            }
            if self.match_t(TokenType::LeftParen) {
                let mut args = Vec::new();
                if !self.check(TokenType::RightParen) {
                    loop {
                        if self.check(TokenType::Identifier)
                            && self.peek_next().token_type == TokenType::Colon
                        {
                            let label_tok = self.advance();
                            let label = label_tok.lexeme.clone();
                            let aloc = self.location_of_prev();
                            self.consume(TokenType::Colon, "Expected ':'")?;
                            let val = self.parse_expression()?;
                            args.push(CallArgument {
                                label: Some(label),
                                value: val,
                                location: aloc,
                            });
                        } else {
                            let val = self.parse_expression()?;
                            let aloc = val.location().clone();
                            args.push(CallArgument {
                                label: None,
                                value: val,
                                location: aloc,
                            });
                        }
                        if !self.match_t(TokenType::Comma) {
                            break;
                        }
                    }
                }
                self.consume(TokenType::RightParen, "Expected ')'")?;
                // Trailing do...end block: `foo(args) do ... end` appends block as last arg
                // Only skip newlines if a `do` actually follows (peek ahead without consuming)
                let saved_index = self.index;
                let saved_doc = self.pending_doc.clone();
                self.skip_newlines();
                if self.check(TokenType::Do) {
                    let block = self.parse_do_block()?;
                    let bloc = block.location().clone();
                    args.push(CallArgument {
                        label: None,
                        value: block,
                        location: bloc,
                    });
                } else {
                    // No trailing do — restore position so newlines aren't consumed
                    self.index = saved_index;
                    self.pending_doc = saved_doc;
                }
                let loc = expr.location().clone();
                expr = Expression::Call(CallExpr {
                    callee: Box::new(expr),
                    args,
                    location: loc,
                });
                continue;
            }
            if self.match_t(TokenType::LeftBracket) {
                let index = self.parse_expression()?;
                self.consume(TokenType::RightBracket, "Expected ']'")?;
                let loc = expr.location().clone();
                expr = Expression::Index(IndexExpr {
                    object: Box::new(expr),
                    index: Box::new(index),
                    location: loc,
                });
                continue;
            }
            if self.match_t(TokenType::Question) {
                let loc = expr.location().clone();
                expr = Expression::OptionalCheck(Box::new(expr), loc);
                continue;
            }
            if self.match_t(TokenType::Bang) {
                let loc = expr.location().clone();
                expr = Expression::ForceUnwrap(Box::new(expr), loc);
                continue;
            }
            break;
        }
        // Trailing do...end after a UFCS member chain:
        // `obj.method do ... end` → method(obj, <closure>) via UFCS rewrite
        // The pre-loop check at the top of this function handles bare
        // identifiers, but the chain `Foo(...).bar do ... end` needs a
        // second check after the .member traversal completes.
        if self.check(TokenType::Do) {
            let is_call_target = matches!(&expr, Expression::Identifier(_) | Expression::Member(_));
            if is_call_target {
                let block = self.parse_do_block()?;
                let bloc = block.location().clone();
                let loc = expr.location().clone();
                expr = Expression::Call(CallExpr {
                    callee: Box::new(expr),
                    args: vec![CallArgument {
                        label: None,
                        value: block,
                        location: bloc,
                    }],
                    location: loc,
                });
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expression, String> {
        // Match identifiers, or keywords used as identifiers in expression context
        // (e.g., `test.assert(...)` where `test` is a module namespace variable)
        if self.match_t(TokenType::Identifier) || self.match_contextual_keyword() {
            let tok = self.previous().clone();
            let loc = SourceLocation {
                line: tok.line,
                column: tok.column,
            };
            return Ok(Expression::Identifier(IdentifierExpr {
                name: tok.lexeme,
                location: loc,
            }));
        }
        // Anonymous function: `def (params) -> ReturnType ... end`
        if self.check(TokenType::Def) && self.peek_next().token_type == TokenType::LeftParen {
            return self.parse_anonymous_function();
        }
        // do...end block as inline closure
        if self.check(TokenType::Do) {
            return self.parse_do_block();
        }
        if self.match_t(TokenType::String) {
            let tok = self.previous().clone();
            let loc = SourceLocation {
                line: tok.line,
                column: tok.column,
            };
            return Ok(Expression::String(StringExpr {
                value: tok.lexeme,
                location: loc,
            }));
        }
        if self.match_t(TokenType::TemplateString) {
            let tok = self.previous().clone();
            return self.parse_template_string_expression(&tok);
        }
        if self.match_t(TokenType::Number) {
            let tok = self.previous().clone();
            let loc = SourceLocation {
                line: tok.line,
                column: tok.column,
            };
            let value: f64 = tok.lexeme.parse().unwrap_or(0.0);
            let is_float = tok.lexeme.contains(['.', 'e', 'E']);
            return Ok(Expression::Number(NumberExpr {
                value,
                is_float,
                location: loc,
            }));
        }
        if self.match_t(TokenType::True) {
            return Ok(Expression::Boolean(BooleanExpr {
                value: true,
                location: self.location_of_prev(),
            }));
        }
        if self.match_t(TokenType::False) {
            return Ok(Expression::Boolean(BooleanExpr {
                value: false,
                location: self.location_of_prev(),
            }));
        }
        if self.match_t(TokenType::Null) {
            return Ok(Expression::Null(self.location_of_prev()));
        }
        if self.match_t(TokenType::LeftBracket) {
            let loc = self.location_of_prev();
            let mut items = Vec::new();
            self.array_depth += 1;
            self.skip_newlines();
            while !self.check(TokenType::RightBracket) {
                items.push(self.parse_expression()?);
                if self.match_t(TokenType::Comma) {
                    self.skip_newlines();
                    continue;
                }
                self.skip_newlines();
            }
            self.array_depth -= 1;
            self.consume(TokenType::RightBracket, "Expected ']'")?;
            let style = if items
                .first()
                .map(|item| item.location().line > loc.line)
                .unwrap_or(false)
            {
                ArrayLiteralStyle::Vertical
            } else {
                ArrayLiteralStyle::Inline
            };
            return Ok(Expression::Array(ArrayExpr {
                items,
                style,
                location: loc,
            }));
        }
        if self.match_t(TokenType::LeftBrace) {
            let loc = self.location_of_prev();
            let mut entries = Vec::new();
            self.skip_newlines();
            while !self.check(TokenType::RightBrace) {
                let key_tok = self.consume_any(
                    &[TokenType::Identifier, TokenType::String],
                    "Expected dict key",
                )?;
                let key = key_tok.lexeme.clone();
                let kloc = self.location_of_tok(&key_tok);
                self.consume(TokenType::Colon, "Expected ':' after dict key")?;
                let value = self.parse_expression()?;
                entries.push(DictionaryEntry {
                    key,
                    value,
                    location: kloc,
                });
                if self.match_t(TokenType::Comma) {
                    self.skip_newlines();
                    continue;
                }
                self.skip_newlines();
            }
            self.consume(TokenType::RightBrace, "Expected '}'")?;
            return Ok(Expression::Dictionary(DictionaryExpr {
                entries,
                location: loc,
            }));
        }
        if self.match_t(TokenType::LeftParen) {
            // Inside parens, reset array context so `-` is binary subtraction again
            let saved_depth = self.array_depth;
            self.array_depth = 0;
            let expr = self.parse_expression()?;
            self.array_depth = saved_depth;
            self.consume(TokenType::RightParen, "Expected ')'")?;
            return Ok(expr);
        }
        Err(format!(
            "Expected expression at {}:{}, found {:?} '{}'",
            self.peek().line,
            self.peek().column,
            self.peek().token_type,
            self.peek().lexeme
        ))
    }

    fn parse_template_string_expression(&mut self, token: &Token) -> Result<Expression, String> {
        let loc = self.location_of_tok(token);
        let raw = &token.lexeme;
        let mut parts = Vec::new();
        let mut cursor = 0;
        let chars: Vec<char> = raw.chars().collect();

        while cursor < chars.len() {
            // Find next {{
            let start = find_substr(&chars, cursor, &['{', '{']);
            if start.is_none() {
                if cursor < chars.len() {
                    parts.push(TemplateStringPart::Text(chars[cursor..].iter().collect()));
                }
                break;
            }
            let start = start.unwrap();
            if start > cursor {
                parts.push(TemplateStringPart::Text(
                    chars[cursor..start].iter().collect(),
                ));
            }
            // Find matching }}
            let end = find_template_expr_end(&chars, start + 2);
            if end.is_none() {
                return Err(format!(
                    "Unterminated template expression at {}:{}",
                    token.line, token.column
                ));
            }
            let end = end.unwrap();
            let expr_src: String = chars[start + 2..end].iter().collect();
            let expr_src = expr_src.trim().to_string();
            if expr_src.is_empty() {
                return Err("Empty template expression".into());
            }
            let tokens = Lexer::new(&expr_src).scan_tokens()?;
            let mut parser = Parser::new(tokens);
            let expr = parser.parse_standalone_expression()?;
            parts.push(TemplateStringPart::Expr(expr));
            cursor = end + 2;
        }

        Ok(Expression::TemplateString(TemplateStringExpr {
            parts,
            location: loc,
        }))
    }

    // ── Helpers ────────────────────────────────────────────────────

    fn match_t(&mut self, tt: TokenType) -> bool {
        if self.check(tt) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn match_any(&mut self, types: &[TokenType]) -> bool {
        for &t in types {
            if self.check(t) {
                self.advance();
                return true;
            }
        }
        false
    }

    /// Match a keyword that's being used as an identifier (e.g., `test.assert()`).
    /// Only matches if the keyword is followed by `.` (member access).
    fn match_contextual_keyword(&mut self) -> bool {
        if self.peek().token_type.is_keyword() && self.peek_next().token_type == TokenType::Dot {
            self.advance();
            return true;
        }
        false
    }

    fn consume(&mut self, tt: TokenType, msg: &str) -> Result<Token, String> {
        if self.check(tt) {
            return Ok(self.advance());
        }
        Err(format!(
            "{} at {}:{}, found {:?} '{}'",
            msg,
            self.peek().line,
            self.peek().column,
            self.peek().token_type,
            self.peek().lexeme
        ))
    }

    fn consume_any(&mut self, types: &[TokenType], msg: &str) -> Result<Token, String> {
        for &t in types {
            if self.check(t) {
                return Ok(self.advance());
            }
        }
        Err(format!(
            "{} at {}:{}",
            msg,
            self.peek().line,
            self.peek().column
        ))
    }

    fn check(&self, tt: TokenType) -> bool {
        !self.is_at_end() && self.peek().token_type == tt
    }

    fn comma_starts_next_parameter(&self) -> bool {
        if !self.check(TokenType::Comma) {
            return false;
        }
        let next = self.tokens.get(self.index + 1);
        let after_next = self.tokens.get(self.index + 2);
        matches!(
            (next.map(|t| t.token_type), after_next.map(|t| t.token_type)),
            (Some(TokenType::Identifier), Some(TokenType::Colon))
        )
    }

    fn is_assignment_start(&self) -> bool {
        if !self.check(TokenType::Identifier) {
            return false;
        }
        let mut offset = 1;
        loop {
            let tt = self
                .tokens
                .get(self.index + offset)
                .map(|t| t.token_type)
                .unwrap_or(TokenType::Eof);
            match tt {
                // Multi-variable: x, y = expr
                TokenType::Comma => {
                    let next = self
                        .tokens
                        .get(self.index + offset + 1)
                        .map(|t| t.token_type)
                        .unwrap_or(TokenType::Eof);
                    if next != TokenType::Identifier {
                        return false;
                    }
                    offset += 2;
                }
                // Field access chain: x.field.nested = expr
                TokenType::Dot => {
                    let next = self
                        .tokens
                        .get(self.index + offset + 1)
                        .map(|t| t.token_type)
                        .unwrap_or(TokenType::Eof);
                    if next != TokenType::Identifier {
                        return false;
                    }
                    offset += 2;
                }
                // Index access: x[i] = expr
                TokenType::LeftBracket => {
                    // Skip to matching ]
                    offset += 1;
                    let mut depth = 1;
                    while depth > 0 {
                        let t = self
                            .tokens
                            .get(self.index + offset)
                            .map(|t| t.token_type)
                            .unwrap_or(TokenType::Eof);
                        if t == TokenType::LeftBracket {
                            depth += 1;
                        }
                        if t == TokenType::RightBracket {
                            depth -= 1;
                        }
                        if t == TokenType::Eof {
                            return false;
                        }
                        offset += 1;
                    }
                }
                _ => break,
            }
        }
        self.tokens
            .get(self.index + offset)
            .map(|t| t.token_type)
            .unwrap_or(TokenType::Eof)
            == TokenType::Assign
    }

    fn advance(&mut self) -> Token {
        if !self.is_at_end() {
            self.index += 1;
        }
        self.previous().clone()
    }

    fn is_at_end(&self) -> bool {
        self.peek().token_type == TokenType::Eof
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn peek_next(&self) -> &Token {
        self.tokens
            .get(self.index + 1)
            .unwrap_or(&self.tokens[self.index])
    }

    fn previous(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn skip_newlines(&mut self) {
        while !self.is_at_end() {
            if self.check(TokenType::Newline) {
                self.advance();
            } else if self.check(TokenType::Comment) {
                let tok = self.advance();
                self.pending_doc.push(tok.lexeme.clone());
            } else {
                break;
            }
        }
    }

    /// Take and clear the accumulated doc comment, returning it as a single string.
    fn take_pending_doc(&mut self) -> Option<String> {
        if self.pending_doc.is_empty() {
            None
        } else {
            let doc = self.pending_doc.join("\n");
            self.pending_doc.clear();
            Some(doc)
        }
    }

    fn location_of_tok(&self, tok: &Token) -> SourceLocation {
        SourceLocation {
            line: tok.line,
            column: tok.column,
        }
    }

    fn location_of_prev(&self) -> SourceLocation {
        let tok = self.previous();
        SourceLocation {
            line: tok.line,
            column: tok.column,
        }
    }

    fn peek_location(&self) -> SourceLocation {
        let tok = self.peek();
        SourceLocation {
            line: tok.line,
            column: tok.column,
        }
    }
}

// ── Template string helpers ────────────────────────────────────────

fn find_substr(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || start + needle.len() > chars.len() {
        return None;
    }
    for i in start..=chars.len() - needle.len() {
        if &chars[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

fn find_template_expr_end(chars: &[char], start: usize) -> Option<usize> {
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut i = start;
    while i < chars.len().saturating_sub(1) {
        let ch = chars[i];
        if ch == '\'' || ch == '"' {
            let quote = ch;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            i += 1;
            continue;
        }
        match ch {
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            '{' => brace += 1,
            '}' => {
                if i + 1 < chars.len()
                    && chars[i + 1] == '}'
                    && paren == 0
                    && bracket == 0
                    && brace == 0
                {
                    return Some(i);
                }
                brace -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}
