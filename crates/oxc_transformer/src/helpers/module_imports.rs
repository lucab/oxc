use std::cell::{Cell, RefCell};

use indexmap::IndexMap;
use oxc_allocator::{Allocator, Vec};
use oxc_ast::{ast::*, AstBuilder};
use oxc_span::{Atom, SPAN};
use oxc_syntax::symbol::SymbolId;

pub struct NamedImport<'a> {
    imported: Atom<'a>,
    local: Option<Atom<'a>>, // Not used in `require`
    symbol_id: SymbolId,
}

impl<'a> NamedImport<'a> {
    pub fn new(imported: Atom<'a>, local: Option<Atom<'a>>, symbol_id: SymbolId) -> Self {
        Self { imported, local, symbol_id }
    }
}

#[derive(Hash, Eq, PartialEq)]
pub enum ImportKind {
    Import,
    Require,
}

#[derive(Hash, Eq, PartialEq)]
pub struct ImportType<'a> {
    kind: ImportKind,
    source: Atom<'a>,
}

impl<'a> ImportType<'a> {
    fn new(kind: ImportKind, source: Atom<'a>) -> Self {
        Self { kind, source }
    }
}

/// Manage import statement globally
/// <https://github.com/nicolo-ribaudo/babel/tree/main/packages/babel-helper-module-imports>
pub struct ModuleImports<'a> {
    ast: AstBuilder<'a>,

    imports: RefCell<IndexMap<ImportType<'a>, std::vec::Vec<NamedImport<'a>>>>,
}

impl<'a> ModuleImports<'a> {
    pub fn new(allocator: &'a Allocator) -> ModuleImports<'a> {
        let ast = AstBuilder::new(allocator);
        Self { ast, imports: RefCell::new(IndexMap::default()) }
    }

    /// Add `import { named_import } from 'source'`
    pub fn add_import(&self, source: Atom<'a>, import: NamedImport<'a>) {
        self.imports
            .borrow_mut()
            .entry(ImportType::new(ImportKind::Import, source))
            .or_default()
            .push(import);
    }

    /// Add `var named_import from 'source'`
    pub fn add_require(&self, source: Atom<'a>, import: NamedImport<'a>, front: bool) {
        let len = self.imports.borrow().len();
        self.imports
            .borrow_mut()
            .entry(ImportType::new(ImportKind::Require, source))
            .or_default()
            .push(import);
        if front {
            self.imports.borrow_mut().move_index(len, 0);
        }
    }

    pub fn get_import_statements(&self) -> Vec<'a, Statement<'a>> {
        self.ast.new_vec_from_iter(self.imports.borrow_mut().drain(..).map(
            |(import_type, names)| match import_type.kind {
                ImportKind::Import => self.get_named_import(import_type.source, names),
                ImportKind::Require => self.get_require(import_type.source, names),
            },
        ))
    }

    fn get_named_import(
        &self,
        source: Atom<'a>,
        names: std::vec::Vec<NamedImport<'a>>,
    ) -> Statement<'a> {
        let specifiers = self.ast.new_vec_from_iter(names.into_iter().map(|name| {
            let local = name.local.unwrap_or_else(|| name.imported.clone());
            ImportDeclarationSpecifier::ImportSpecifier(self.ast.alloc(ImportSpecifier {
                span: SPAN,
                imported: ModuleExportName::IdentifierName(IdentifierName::new(
                    SPAN,
                    name.imported,
                )),
                local: BindingIdentifier {
                    span: SPAN,
                    name: local,
                    symbol_id: Cell::new(Some(name.symbol_id)),
                },
                import_kind: ImportOrExportKind::Value,
            }))
        }));
        let import_stmt = self.ast.import_declaration(
            SPAN,
            Some(specifiers),
            StringLiteral::new(SPAN, source),
            None,
            ImportOrExportKind::Value,
        );
        self.ast.module_declaration(ModuleDeclaration::ImportDeclaration(import_stmt))
    }

    fn get_require(
        &self,
        source: Atom<'a>,
        names: std::vec::Vec<NamedImport<'a>>,
    ) -> Statement<'a> {
        let var_kind = VariableDeclarationKind::Var;
        let callee = {
            let ident = IdentifierReference::new(SPAN, Atom::from("require"));
            self.ast.identifier_reference_expression(ident)
        };
        let args = {
            let string = StringLiteral::new(SPAN, source);
            let arg = Argument::from(self.ast.literal_string_expression(string));
            self.ast.new_vec_single(arg)
        };
        let name = names.into_iter().next().unwrap();
        let id = {
            let ident = BindingIdentifier {
                span: SPAN,
                name: name.imported,
                symbol_id: Cell::new(Some(name.symbol_id)),
            };
            self.ast.binding_pattern(SPAN, self.ast.binding_pattern_identifier(ident), None, false)
        };
        let decl = {
            let init = self.ast.call_expression(SPAN, callee, args, false, None);
            let decl = self.ast.variable_declarator(SPAN, var_kind, id, Some(init), false);
            self.ast.new_vec_single(decl)
        };
        let var_decl = self.ast.variable_declaration(SPAN, var_kind, decl, false);
        Statement::VariableDeclaration(var_decl)
    }
}
