// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module defines the transfer functions for verifying reference safety of a procedure body.
//! The checks include (but are not limited to)
//! - verifying that there are no dangling references,
//! - accesses to mutable references are safe
//! - accesses to global storage references are safe

mod abstract_state;

use crate::{
    absint::{AbstractInterpreter, TransferFunctions},
    meter::{Meter, Scope},
    set_based_reference_safety::abstract_state::{
        STEP_BASE_COST, STEP_PER_COLLECTION_ITEM_COST, STEP_PER_LOCAL_COST,
    },
};
use abstract_state::{AbstractState, AbstractValue};
use move_abstract_stack::AbstractStack;
use move_binary_format::{
    binary_views::{BinaryIndexedView, FunctionView},
    errors::{PartialVMError, PartialVMResult},
    file_format::{
        Bytecode, CodeOffset, FunctionDefinitionIndex, FunctionHandle, IdentifierIndex,
        StructDefinition, StructFieldInformation,
    },
    safe_assert, safe_unwrap, safe_unwrap_err,
};
use move_core_types::vm_status::StatusCode;
use std::{
    collections::{BTreeSet, HashMap},
    num::NonZeroU64,
};

use self::abstract_state::ValueKind;

struct ReferenceSafetyAnalysis<'a> {
    resolver: &'a BinaryIndexedView<'a>,
    function_view: &'a FunctionView<'a>,
    name_def_map: &'a HashMap<IdentifierIndex, FunctionDefinitionIndex>,
    stack: AbstractStack<AbstractValue>,
}

impl<'a> ReferenceSafetyAnalysis<'a> {
    fn new(
        resolver: &'a BinaryIndexedView<'a>,
        function_view: &'a FunctionView<'a>,
        name_def_map: &'a HashMap<IdentifierIndex, FunctionDefinitionIndex>,
    ) -> Self {
        Self {
            resolver,
            function_view,
            name_def_map,
            stack: AbstractStack::new(),
        }
    }

    fn push(&mut self, v: AbstractValue) -> PartialVMResult<()> {
        safe_unwrap_err!(self.stack.push(v));
        Ok(())
    }

    fn push_n(&mut self, v: AbstractValue, n: u64) -> PartialVMResult<()> {
        safe_unwrap_err!(self.stack.push_n(v, n));
        Ok(())
    }
}

pub(crate) fn verify<'a>(
    resolver: &'a BinaryIndexedView<'a>,
    function_view: &FunctionView,
    name_def_map: &'a HashMap<IdentifierIndex, FunctionDefinitionIndex>,
    meter: &mut impl Meter,
) -> PartialVMResult<()> {
    let initial_state = AbstractState::new(function_view);

    let mut verifier = ReferenceSafetyAnalysis::new(resolver, function_view, name_def_map);
    verifier.analyze_function(initial_state, function_view, meter)
}

fn call(
    verifier: &mut ReferenceSafetyAnalysis,
    state: &mut AbstractState,
    offset: CodeOffset,
    function_handle: &FunctionHandle,
    meter: &mut impl Meter,
) -> PartialVMResult<()> {
    let parameters = verifier.resolver.signature_at(function_handle.parameters);
    let arguments = parameters
        .0
        .iter()
        .map(|_| verifier.stack.pop().unwrap())
        .rev()
        .collect();

    let acquired_resources = match verifier.name_def_map.get(&function_handle.name) {
        Some(idx) => {
            let func_def = verifier.resolver.function_def_at(*idx)?;
            let fh = verifier.resolver.function_handle_at(func_def.function);
            if function_handle == fh {
                func_def.acquires_global_resources.iter().cloned().collect()
            } else {
                BTreeSet::new()
            }
        }
        None => BTreeSet::new(),
    };
    let return_ = verifier.resolver.signature_at(function_handle.return_);
    let return_kinds = ValueKind::for_signature(return_);
    let values = state.call(
        offset,
        arguments,
        &acquired_resources,
        &return_kinds,
        meter,
        StatusCode::CALL_BORROWED_MUTABLE_REFERENCE_ERROR,
    )?;
    for value in values {
        verifier.push(value)?
    }
    Ok(())
}

fn num_fields(struct_def: &StructDefinition) -> usize {
    match &struct_def.field_information {
        StructFieldInformation::Native => 0,
        StructFieldInformation::Declared(fields) => fields.len(),
    }
}

fn pack(
    verifier: &mut ReferenceSafetyAnalysis,
    struct_def: &StructDefinition,
) -> PartialVMResult<()> {
    for _ in 0..num_fields(struct_def) {
        safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value())
    }
    // TODO maybe call state.value_for
    verifier.push(AbstractValue::NonReference)?;
    Ok(())
}

fn unpack(
    verifier: &mut ReferenceSafetyAnalysis,
    struct_def: &StructDefinition,
) -> PartialVMResult<()> {
    safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
    // TODO maybe call state.value_for
    verifier.push_n(AbstractValue::NonReference, num_fields(struct_def) as u64)?;
    Ok(())
}

fn execute_inner(
    verifier: &mut ReferenceSafetyAnalysis,
    state: &mut AbstractState,
    bytecode: &Bytecode,
    offset: CodeOffset,
    meter: &mut impl Meter,
) -> PartialVMResult<()> {
    meter.add(Scope::Function, STEP_BASE_COST)?;
    meter.add_items(Scope::Function, STEP_PER_LOCAL_COST, state.local_count())?;
    meter.add_items(
        Scope::Function,
        STEP_PER_COLLECTION_ITEM_COST,
        state.total_reference_size(),
    )?;

    match bytecode {
        Bytecode::Pop => state.release_value(safe_unwrap_err!(verifier.stack.pop())),

        Bytecode::CopyLoc(local) => {
            let value = state.copy_loc(offset, *local)?;
            verifier.push(value)?
        }
        Bytecode::MoveLoc(local) => {
            let value = state.move_loc(offset, *local)?;
            verifier.push(value)?
        }
        Bytecode::StLoc(local) => {
            state.st_loc(offset, *local, safe_unwrap_err!(verifier.stack.pop()))?
        }

        Bytecode::FreezeRef => {
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let frozen = state.freeze_ref(offset, id)?;
            verifier.push(frozen)?
        }
        Bytecode::Eq | Bytecode::Neq => {
            let v1 = safe_unwrap_err!(verifier.stack.pop());
            let v2 = safe_unwrap_err!(verifier.stack.pop());
            let value = state.comparison(offset, v1, v2)?;
            verifier.push(value)?
        }
        Bytecode::ReadRef => {
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let value = state.read_ref(offset, id)?;
            verifier.push(value)?
        }
        Bytecode::WriteRef => {
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let val_operand = safe_unwrap_err!(verifier.stack.pop());
            safe_assert!(val_operand.is_value());
            state.write_ref(offset, id)?
        }

        Bytecode::MutBorrowLoc(local) => {
            let value = state.borrow_loc(offset, true, *local)?;
            verifier.push(value)?
        }
        Bytecode::ImmBorrowLoc(local) => {
            let value = state.borrow_loc(offset, false, *local)?;
            verifier.push(value)?
        }
        Bytecode::MutBorrowField(field_handle_index) => {
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let value = state.borrow_field(offset, true, id, *field_handle_index)?;
            verifier.push(value)?
        }
        Bytecode::MutBorrowFieldGeneric(field_inst_index) => {
            let field_inst = verifier
                .resolver
                .field_instantiation_at(*field_inst_index)?;
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let value = state.borrow_field(offset, true, id, field_inst.handle)?;
            verifier.push(value)?
        }
        Bytecode::ImmBorrowField(field_handle_index) => {
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let value = state.borrow_field(offset, false, id, *field_handle_index)?;
            verifier.push(value)?
        }
        Bytecode::ImmBorrowFieldGeneric(field_inst_index) => {
            let field_inst = verifier
                .resolver
                .field_instantiation_at(*field_inst_index)?;
            let id = safe_unwrap!(safe_unwrap_err!(verifier.stack.pop()).ref_id());
            let value = state.borrow_field(offset, false, id, field_inst.handle)?;
            verifier.push(value)?
        }

        Bytecode::MutBorrowGlobal(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let value = state.borrow_global(offset, true, *idx)?;
            verifier.push(value)?
        }
        Bytecode::MutBorrowGlobalGeneric(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let struct_inst = verifier.resolver.struct_instantiation_at(*idx)?;
            let value = state.borrow_global(offset, true, struct_inst.def)?;
            verifier.push(value)?
        }
        Bytecode::ImmBorrowGlobal(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let value = state.borrow_global(offset, false, *idx)?;
            verifier.push(value)?
        }
        Bytecode::ImmBorrowGlobalGeneric(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let struct_inst = verifier.resolver.struct_instantiation_at(*idx)?;
            let value = state.borrow_global(offset, false, struct_inst.def)?;
            verifier.push(value)?
        }
        Bytecode::MoveFrom(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let value = state.move_from(offset, *idx)?;
            verifier.push(value)?
        }
        Bytecode::MoveFromGeneric(idx) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let struct_inst = verifier.resolver.struct_instantiation_at(*idx)?;
            let value = state.move_from(offset, struct_inst.def)?;
            verifier.push(value)?
        }

        Bytecode::Call(idx) => {
            let function_handle = verifier.resolver.function_handle_at(*idx);
            call(verifier, state, offset, function_handle, meter)?
        }
        Bytecode::CallGeneric(idx) => {
            let func_inst = verifier.resolver.function_instantiation_at(*idx);
            let function_handle = verifier.resolver.function_handle_at(func_inst.handle);
            call(verifier, state, offset, function_handle, meter)?
        }

        Bytecode::Ret => {
            let mut return_values = vec![];
            for _ in 0..verifier.function_view.return_().len() {
                return_values.push(safe_unwrap_err!(verifier.stack.pop()));
            }
            return_values.reverse();

            state.ret(offset, return_values)?
        }

        Bytecode::Branch(_)
        | Bytecode::Nop
        | Bytecode::CastU8
        | Bytecode::CastU16
        | Bytecode::CastU32
        | Bytecode::CastU64
        | Bytecode::CastU128
        | Bytecode::CastU256
        | Bytecode::Not
        | Bytecode::Exists(_)
        | Bytecode::ExistsGeneric(_) => (),

        Bytecode::BrTrue(_) | Bytecode::BrFalse(_) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
        }
        Bytecode::Abort => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            state.abort()
        }
        Bytecode::MoveTo(_) | Bytecode::MoveToGeneric(_) => {
            // resource value
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            // signer reference
            state.release_value(safe_unwrap_err!(verifier.stack.pop()));
        }

        Bytecode::LdTrue
        | Bytecode::LdFalse
        | Bytecode::LdU8(_)
        | Bytecode::LdU16(_)
        | Bytecode::LdU32(_)
        | Bytecode::LdU64(_)
        | Bytecode::LdU128(_)
        | Bytecode::LdU256(_)
        | Bytecode::LdConst(_) => verifier.push(AbstractValue::NonReference)?,

        Bytecode::Add
        | Bytecode::Sub
        | Bytecode::Mul
        | Bytecode::Mod
        | Bytecode::Div
        | Bytecode::BitOr
        | Bytecode::BitAnd
        | Bytecode::Xor
        | Bytecode::Shl
        | Bytecode::Shr
        | Bytecode::Or
        | Bytecode::And
        | Bytecode::Lt
        | Bytecode::Gt
        | Bytecode::Le
        | Bytecode::Ge => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            // TODO maybe call state.value_for
            verifier.push(AbstractValue::NonReference)?
        }

        Bytecode::Pack(idx) => {
            let struct_def = verifier.resolver.struct_def_at(*idx)?;
            pack(verifier, struct_def)?
        }
        Bytecode::PackGeneric(idx) => {
            let struct_inst = verifier.resolver.struct_instantiation_at(*idx)?;
            let struct_def = verifier.resolver.struct_def_at(struct_inst.def)?;
            pack(verifier, struct_def)?
        }
        Bytecode::Unpack(idx) => {
            let struct_def = verifier.resolver.struct_def_at(*idx)?;
            unpack(verifier, struct_def)?
        }
        Bytecode::UnpackGeneric(idx) => {
            let struct_inst = verifier.resolver.struct_instantiation_at(*idx)?;
            let struct_def = verifier.resolver.struct_def_at(struct_inst.def)?;
            unpack(verifier, struct_def)?
        }

        Bytecode::VecPack(_, num) => {
            if let Some(num_to_pop) = NonZeroU64::new(*num) {
                let result = verifier.stack.pop_eq_n(num_to_pop);
                let abs_value = safe_unwrap_err!(result);
                safe_assert!(abs_value.is_value());
            }

            verifier.push(AbstractValue::NonReference)?;
        }

        Bytecode::VecLen(_) => {
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            state.vector_op(offset, vec_ref, false)?;
            verifier.push(AbstractValue::NonReference)?;
        }

        Bytecode::VecImmBorrow(_) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            let values = state.call(
                offset,
                vec![vec_ref],
                &BTreeSet::new(),
                &[ValueKind::Reference(false)],
                meter,
                StatusCode::VEC_BORROW_ELEMENT_EXISTS_MUTABLE_BORROW_ERROR, // should not be hit
            )?;
            debug_assert!(values.len() == 1);
            for value in values {
                verifier.push(value)?
            }
        }
        Bytecode::VecMutBorrow(_) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            let values = state.call(
                offset,
                vec![vec_ref],
                &BTreeSet::new(),
                &[ValueKind::Reference(true)],
                meter,
                StatusCode::VEC_BORROW_ELEMENT_EXISTS_MUTABLE_BORROW_ERROR,
            )?;
            debug_assert!(values.len() == 1);
            for value in values {
                verifier.push(value)?
            }
        }

        Bytecode::VecPushBack(_) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            state.vector_op(offset, vec_ref, true)?;
        }

        Bytecode::VecPopBack(_) => {
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            state.vector_op(offset, vec_ref, true)?;

            verifier.push(AbstractValue::NonReference)?
        }

        Bytecode::VecUnpack(_, num) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());

            verifier.push_n(AbstractValue::NonReference, *num)?
        }

        Bytecode::VecSwap(_) => {
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            safe_assert!(safe_unwrap_err!(verifier.stack.pop()).is_value());
            let vec_ref = safe_unwrap_err!(verifier.stack.pop());
            state.vector_op(offset, vec_ref, true)?;
        }
    };
    Ok(())
}

impl<'a> TransferFunctions for ReferenceSafetyAnalysis<'a> {
    type State = AbstractState;
    type Error = PartialVMError;

    fn execute(
        &mut self,
        state: &mut Self::State,
        bytecode: &Bytecode,
        index: CodeOffset,
        last_index: CodeOffset,
        meter: &mut impl Meter,
    ) -> PartialVMResult<()> {
        execute_inner(self, state, bytecode, index, meter)?;
        debug_assert!(state.satisfies_invariant(), "after {bytecode:?}");
        if index == last_index {
            safe_assert!(self.stack.is_empty());
            state.canonicalize()
        }
        Ok(())
    }
}

impl<'a> AbstractInterpreter for ReferenceSafetyAnalysis<'a> {}
