//! ES2022: Class Properties
//! Transform of static property initializers.

use std::cell::Cell;

use oxc_ast::{
    ast::*,
    visit::{walk_mut, VisitMut},
};
use oxc_syntax::scope::{ScopeFlags, ScopeId};
use oxc_traverse::TraverseCtx;

use super::ClassProperties;

impl<'a, 'ctx> ClassProperties<'a, 'ctx> {
    /// Transform static property initializer.
    ///
    /// Replace `this`, and references to class name, with temp var for class.
    /// See below for full details of transforms.
    pub(super) fn transform_static_initializer(
        &mut self,
        value: &mut Expression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        let mut replacer = StaticInitializerVisitor::new(self, ctx);
        replacer.visit_expression(value);
    }
}

/// Visitor to transform:
///
/// 1. `this` to class temp var.
///    * Class declaration: `class C { static x = this.y; }`
///      -> `var _C; class C {}; _C = C; C.x = _C.y;`
///    * Class expression: `x = class C { static x = this.y; }`
///      -> `var _C; x = (_C = class C {}, _C.x = _C.y, _C)`
/// 2. Reference to class name to class temp var.
///    * Class declaration: `class C { static x = C.y; }`
///      -> `var _C; class C {}; _C = C; C.x = _C.y;`
///    * Class expression: `x = class C { static x = C.y; }`
///      -> `var _C; x = (_C = class C {}, _C.x = _C.y, _C)`
///
/// Also:
/// * Update parent `ScopeId` of first level of scopes in initializer.
/// * Set `ScopeFlags` of scopes to sloppy mode if code outside the class is sloppy mode.
///
/// Reason we need to transform `this` is because the initializer is being moved from inside the class
/// to outside. `this` outside the class refers to a different `this`. So we need to transform it.
///
/// Note that for class declarations, assignments are made to properties of original class name `C`,
/// but temp var `_C` is used in replacements for `this` or class name.
/// This is because class binding `C` could be mutated, and the initializer may contain functions which
/// are not executed immediately, so the mutation occurs before that initializer code runs.
///
/// ```js
/// class C {
///   static getSelf = () => this;
///   static getSelf2 = () => C;
/// }
/// const C2 = C;
/// C = 123;
/// assert(C2.getSelf() === C); // Would fail if `this` was replaced with `C`, instead of temp var
/// assert(C2.getSelf2() === C); // Would fail if `C` in `getSelf2` was not replaced with temp var
/// ```
///
/// If this class has no name, and no `ScopeFlags` need updating, then we only need to transform `this`,
/// and re-parent first-level scopes. So can skip traversing into functions and other contexts which have
/// their own `this`.
//
// TODO(improve-on-babel): Unnecessary to create temp var for class declarations if either:
// 1. Class name binding is not mutated.
// 2. `this` / reference to class name / private field is not in a nested function, so we know the
//    code runs immediately, before any mutation of the class name binding can occur.
//
// TODO(improve-on-babel): Updating `ScopeFlags` for strict mode makes semantic correctly for the output,
// but actually the transform isn't right. Should wrap initializer in a strict mode IIFE so that
// initializer code runs in strict mode, as it was before within class body.
struct StaticInitializerVisitor<'a, 'ctx, 'v> {
    /// `true` if class has name, or `ScopeFlags` need updating.
    /// Either of these neccesitates walking the whole tree. If neither applies, we only need to walk
    /// as far as functions and other constructs which define a `this`.
    walk_deep: bool,
    /// `true` if should make scopes sloppy mode
    make_sloppy_mode: bool,
    /// Incremented when entering a different `this` context, decremented when exiting it.
    /// `this` should be transformed when `this_depth == 0`.
    this_depth: u32,
    /// Incremented when entering scope, decremented when exiting it.
    /// Parent `ScopeId` should be updated when `scope_depth == 0`.
    /// Note: `scope_depth` does not aim to track scope depth completely accurately.
    /// Only requirement is to ensure that `scope_depth == 0` only when we're in first-level scope.
    /// So we don't bother incrementing + decrementing for scopes which are definitely not first level.
    /// e.g. `BlockStatement` or `ForStatement` must be in a function, and therefore we're already in a
    /// nested scope.
    scope_depth: u32,
    /// Main transform instance.
    class_properties: &'v mut ClassProperties<'a, 'ctx>,
    /// `TraverseCtx` object.
    ctx: &'v mut TraverseCtx<'a>,
}

impl<'a, 'ctx, 'v> StaticInitializerVisitor<'a, 'ctx, 'v> {
    fn new(
        class_properties: &'v mut ClassProperties<'a, 'ctx>,
        ctx: &'v mut TraverseCtx<'a>,
    ) -> Self {
        let make_sloppy_mode = !ctx.current_scope_flags().is_strict_mode();
        let walk_deep =
            make_sloppy_mode || class_properties.current_class().bindings.name.is_some();

        Self { walk_deep, make_sloppy_mode, this_depth: 0, scope_depth: 0, class_properties, ctx }
    }
}

impl<'a, 'ctx, 'v> VisitMut<'a> for StaticInitializerVisitor<'a, 'ctx, 'v> {
    #[inline]
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        match expr {
            // `this`
            Expression::ThisExpression(this_expr) => {
                let span = this_expr.span;
                self.replace_this_with_temp_var(expr, span);
                return;
            }
            // `delete this`
            Expression::UnaryExpression(unary_expr) => {
                if unary_expr.operator == UnaryOperator::Delete
                    && matches!(&unary_expr.argument, Expression::ThisExpression(_))
                {
                    let span = unary_expr.span;
                    self.replace_delete_this_with_true(expr, span);
                    return;
                }
            }
            // `super.prop`
            Expression::StaticMemberExpression(_) => {
                self.transform_static_member_expression_if_super(expr);
            }
            // `super[prop]`
            Expression::ComputedMemberExpression(_) => {
                self.transform_computed_member_expression_if_super(expr);
            }
            // `super.prop()`
            Expression::CallExpression(call_expr) => {
                self.transform_call_expression_if_super_member_expression(call_expr);
            }
            // `super.prop = value`, `super.prop += value`, `super.prop ??= value`
            Expression::AssignmentExpression(_) => {
                self.transform_assignment_expression_if_super_member_assignment_target(expr);
            }
            // `super.prop++`, `--super.prop`
            Expression::UpdateExpression(_) => {
                self.transform_update_expression_if_super_member_assignment_target(expr);
            }
            _ => {}
        }

        walk_mut::walk_expression(self, expr);
    }

    /// Transform reference to class name to temp var
    fn visit_identifier_reference(&mut self, ident: &mut IdentifierReference<'a>) {
        self.replace_class_name_with_temp_var(ident);
    }

    /// Convert scope to sloppy mode if `self.make_sloppy_mode == true`.
    // `#[inline]` because called from many `walk` functions and is small.
    #[inline]
    fn enter_scope(&mut self, _flags: ScopeFlags, scope_id: &Cell<Option<ScopeId>>) {
        if self.make_sloppy_mode {
            let scope_id = scope_id.get().unwrap();
            *self.ctx.scopes_mut().get_flags_mut(scope_id) -= ScopeFlags::StrictMode;
        }
    }

    // Increment `this_depth` when entering code where `this` refers to a different `this`
    // from `this` within this class, and decrement it when exiting.
    // Therefore `this_depth == 0` when `this` refers to the `this` which needs to be transformed.
    //
    // Or, if class has no name, and `ScopeFlags` don't need updating, stop traversing entirely.
    // No scopes need flags updating, so no point searching for them.
    //
    // Also set `make_sloppy_mode = false` while traversing a construct which is strict mode.

    #[inline]
    fn visit_function(&mut self, func: &mut Function<'a>, flags: ScopeFlags) {
        let parent_sloppy_mode = self.make_sloppy_mode;
        if self.make_sloppy_mode && func.has_use_strict_directive() {
            // Function has a `"use strict"` directive in body
            self.make_sloppy_mode = false;
        }

        self.reparent_scope_if_first_level(&func.scope_id);

        if self.walk_deep {
            self.this_depth += 1;
            self.scope_depth += 1;
            walk_mut::walk_function(self, func, flags);
            self.this_depth -= 1;
            self.scope_depth -= 1;
        }

        self.make_sloppy_mode = parent_sloppy_mode;
    }

    #[inline]
    fn visit_arrow_function_expression(&mut self, func: &mut ArrowFunctionExpression<'a>) {
        let parent_sloppy_mode = self.make_sloppy_mode;
        if self.make_sloppy_mode && func.has_use_strict_directive() {
            // Arrow function has a `"use strict"` directive in body
            self.make_sloppy_mode = false;
        }

        self.reparent_scope_if_first_level(&func.scope_id);

        self.scope_depth += 1;
        walk_mut::walk_arrow_function_expression(self, func);
        self.scope_depth -= 1;

        self.make_sloppy_mode = parent_sloppy_mode;
    }

    #[inline]
    fn visit_class(&mut self, class: &mut Class<'a>) {
        let parent_sloppy_mode = self.make_sloppy_mode;
        // Classes are always strict mode
        self.make_sloppy_mode = false;

        self.reparent_scope_if_first_level(&class.scope_id);

        self.scope_depth += 1;
        walk_mut::walk_class(self, class);
        self.scope_depth -= 1;

        self.make_sloppy_mode = parent_sloppy_mode;
    }

    #[inline]
    fn visit_static_block(&mut self, block: &mut StaticBlock<'a>) {
        // Not possible that `self.scope_depth == 0` here, because a `StaticBlock`
        // can only be in a class, and that class would be the first-level scope.
        // So no need to call `reparent_scope_if_first_level`.

        // `walk_deep` must be `true` or we couldn't get here, because a `StaticBlock`
        // must be in a class, and traversal would have stopped in `visit_class` if it wasn't
        self.this_depth += 1;
        walk_mut::walk_static_block(self, block);
        self.this_depth -= 1;
    }

    #[inline]
    fn visit_ts_module_block(&mut self, block: &mut TSModuleBlock<'a>) {
        // Not possible that `self.scope_depth == 0` here, because a `TSModuleBlock`
        // can only be in a function, and that function would be the first-level scope.
        // So no need to call `reparent_scope_if_first_level`.

        let parent_sloppy_mode = self.make_sloppy_mode;
        if self.make_sloppy_mode && block.has_use_strict_directive() {
            // Block has a `"use strict"` directive in body
            self.make_sloppy_mode = false;
        }

        if self.walk_deep {
            self.this_depth += 1;
            walk_mut::walk_ts_module_block(self, block);
            self.this_depth -= 1;
        }

        self.make_sloppy_mode = parent_sloppy_mode;
    }

    #[inline]
    fn visit_property_definition(&mut self, prop: &mut PropertyDefinition<'a>) {
        // `this` in computed key of property or method refers to `this` of parent class.
        // So visit computed `key` within current `this` scope,
        // but increment `this_depth` before visiting `value`.
        // ```js
        // class Outer {
        //   static prop = class Inner { [this] = 1; };
        // }
        // ```
        // Don't visit `type_annotation` field because can't contain `this`.

        // Not possible that `self.scope_depth == 0` here, because a `PropertyDefinition`
        // can only be in a class, and that class would be the first-level scope.
        // So no need to call `reparent_scope_if_first_level`.

        // TODO: Are decorators in scope?
        self.visit_decorators(&mut prop.decorators);
        if prop.computed {
            self.visit_property_key(&mut prop.key);
        }

        // `walk_deep` must be `true` or we couldn't get here, because a `PropertyDefinition`
        // must be in a class, and traversal would have stopped in `visit_class` if it wasn't
        if let Some(value) = &mut prop.value {
            self.this_depth += 1;
            self.visit_expression(value);
            self.this_depth -= 1;
        }
    }

    #[inline]
    fn visit_accessor_property(&mut self, prop: &mut AccessorProperty<'a>) {
        // Not possible that `self.scope_depth == 0` here, because an `AccessorProperty`
        // can only be in a class, and that class would be the first-level scope.
        // So no need to call `reparent_scope_if_first_level`.

        // Treat `key` and `value` in same way as `visit_property_definition` above.
        // TODO: Are decorators in scope?
        self.visit_decorators(&mut prop.decorators);
        if prop.computed {
            self.visit_property_key(&mut prop.key);
        }

        // `walk_deep` must be `true` or we couldn't get here, because an `AccessorProperty`
        // must be in a class, and traversal would have stopped in `visit_class` if it wasn't
        if let Some(value) = &mut prop.value {
            self.this_depth += 1;
            self.visit_expression(value);
            self.this_depth -= 1;
        }
    }

    // Remaining visitors are the only other types which have a scope which can be first-level
    // when starting traversal from an `Expression`.
    // `BlockStatement` and all other statements would need to be within a function,
    // and that function would be the first-level scope.

    #[inline]
    fn visit_ts_conditional_type(&mut self, conditional: &mut TSConditionalType<'a>) {
        self.reparent_scope_if_first_level(&conditional.scope_id);

        // `check_type` field is outside `TSConditionalType`'s scope
        self.visit_ts_type(&mut conditional.check_type);

        self.enter_scope(ScopeFlags::empty(), &conditional.scope_id);

        self.scope_depth += 1;
        self.visit_ts_type(&mut conditional.extends_type);
        self.visit_ts_type(&mut conditional.true_type);
        self.scope_depth -= 1;

        // `false_type` field is outside `TSConditionalType`'s scope
        self.visit_ts_type(&mut conditional.false_type);
    }

    #[inline]
    fn visit_ts_method_signature(&mut self, signature: &mut TSMethodSignature<'a>) {
        self.reparent_scope_if_first_level(&signature.scope_id);

        self.scope_depth += 1;
        walk_mut::walk_ts_method_signature(self, signature);
        self.scope_depth -= 1;
    }

    #[inline]
    fn visit_ts_construct_signature_declaration(
        &mut self,
        signature: &mut TSConstructSignatureDeclaration<'a>,
    ) {
        self.reparent_scope_if_first_level(&signature.scope_id);

        self.scope_depth += 1;
        walk_mut::walk_ts_construct_signature_declaration(self, signature);
        self.scope_depth -= 1;
    }

    #[inline]
    fn visit_ts_mapped_type(&mut self, mapped: &mut TSMappedType<'a>) {
        self.reparent_scope_if_first_level(&mapped.scope_id);

        self.scope_depth += 1;
        walk_mut::walk_ts_mapped_type(self, mapped);
        self.scope_depth -= 1;
    }
}

impl<'a, 'ctx, 'v> StaticInitializerVisitor<'a, 'ctx, 'v> {
    /// Replace `this` with reference to temp var for class.
    fn replace_this_with_temp_var(&mut self, expr: &mut Expression<'a>, span: Span) {
        if self.this_depth == 0 {
            let class_details = self.class_properties.current_class_mut();
            let temp_binding = class_details.bindings.get_or_init_static_binding(self.ctx);
            *expr = temp_binding.create_spanned_read_expression(span, self.ctx);
        }
    }

    /// Replace reference to class name with reference to temp var for class.
    fn replace_class_name_with_temp_var(&mut self, ident: &mut IdentifierReference<'a>) {
        // Check identifier is reference to class name
        let class_details = self.class_properties.current_class_mut();
        let class_name_symbol_id = class_details.bindings.name_symbol_id();
        let Some(class_name_symbol_id) = class_name_symbol_id else { return };

        let reference_id = ident.reference_id();
        let reference = self.ctx.symbols().get_reference(reference_id);
        let Some(symbol_id) = reference.symbol_id() else { return };

        if symbol_id != class_name_symbol_id {
            return;
        }

        // Identifier is reference to class name. Rename it.
        let temp_binding = class_details.bindings.get_or_init_static_binding(self.ctx);
        ident.name = temp_binding.name.clone();

        let symbols = self.ctx.symbols_mut();
        symbols.get_reference_mut(reference_id).set_symbol_id(temp_binding.symbol_id);
        symbols.delete_resolved_reference(symbol_id, reference_id);
        symbols.add_resolved_reference(temp_binding.symbol_id, reference_id);
    }

    /// Replace `delete this` with `true`.
    fn replace_delete_this_with_true(&self, expr: &mut Expression<'a>, span: Span) {
        if self.this_depth == 0 {
            *expr = self.ctx.ast.expression_boolean_literal(span, true);
        }
    }

    /// Update parent of scope to scope above class if this is a first-level scope.
    fn reparent_scope_if_first_level(&mut self, scope_id: &Cell<Option<ScopeId>>) {
        if self.scope_depth == 0 {
            let scope_id = scope_id.get().unwrap();
            let current_scope_id = self.ctx.current_scope_id();
            self.ctx.scopes_mut().change_parent_id(scope_id, Some(current_scope_id));
        }
    }

    // `#[inline]` into visitor to keep common path where member expression isn't `super.prop` fast
    #[inline]
    fn transform_static_member_expression_if_super(&mut self, expr: &mut Expression<'a>) {
        if self.this_depth == 0 {
            self.class_properties.transform_static_member_expression(expr, self.ctx);
        }
    }

    // `#[inline]` into visitor to keep common path where member expression isn't `super.prop` fast
    #[inline]
    fn transform_computed_member_expression_if_super(&mut self, expr: &mut Expression<'a>) {
        if self.this_depth == 0 {
            self.class_properties.transform_computed_member_expression(expr, self.ctx);
        }
    }

    // `#[inline]` into visitor to keep common path where call expression isn't `super.prop()` fast
    #[inline]
    fn transform_call_expression_if_super_member_expression(
        &mut self,
        call_expr: &mut CallExpression<'a>,
    ) {
        if self.this_depth == 0 {
            self.class_properties
                .transform_call_expression_for_super_member_expr(call_expr, self.ctx);
        }
    }

    // `#[inline]` into visitor to keep common path where assignment expression isn't
    // `super.prop += 1` fast
    #[inline]
    fn transform_assignment_expression_if_super_member_assignment_target(
        &mut self,
        expr: &mut Expression<'a>,
    ) {
        if self.this_depth == 0 {
            self.class_properties
                .transform_assignment_expression_for_super_assignment_target(expr, self.ctx);
        }
    }

    // `#[inline]` into visitor to keep common path where assignment expression isn't
    // `super.prop++` fast
    #[inline]
    fn transform_update_expression_if_super_member_assignment_target(
        &mut self,
        expr: &mut Expression<'a>,
    ) {
        if self.this_depth == 0 {
            self.class_properties
                .transform_update_expression_for_super_assignment_target(expr, self.ctx);
        }
    }
}
