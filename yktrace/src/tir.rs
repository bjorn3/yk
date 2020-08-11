//! Conceptually this module takes an ordered collection of SIR block locations and converts it
//! into a Tracing IR (TIR) Trace using the SIR found in the `.yk_sir` section of the currently
//! running executable.

use super::SirTrace;
use crate::{errors::InvalidTraceError, sir::SIR};
use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::{self, Display}
};
use ykpack::Terminator;
pub use ykpack::{
    BinOp, CallOperand, Constant, ConstantInt, Local, LocalDecl, LocalIndex, Operand, Place,
    PlaceBase, Projection, Rvalue, SignedInt, Statement, UnsignedInt
};

/// A TIR trace is conceptually a straight-line path through the SIR with guarded speculation.
#[derive(Debug)]
pub struct TirTrace {
    ops: Vec<TirOp>,
    trace_inputs_local: Option<Local>,
    /// Maps each local variable to its declaration, including type.
    pub local_decls: HashMap<Local, LocalDecl>
}

impl TirTrace {
    /// Create a TirTrace from a SirTrace, trimming remnants of the code which starts/stops the
    /// tracer. Returns a TIR trace and the bounds the SIR trace was trimmed to, or Err if a symbol
    /// is encountered for which no SIR is available.
    pub fn new<'s>(trace: &'s dyn SirTrace) -> Result<Self, InvalidTraceError> {
        let mut ops = Vec::new();
        let mut itr = trace.into_iter().peekable();
        let mut rnm = VarRenamer::new();
        let mut trace_inputs_local: Option<Local> = None;

        while let Some(loc) = itr.next() {
            let body = match SIR.bodies.get(&loc.symbol_name) {
                Some(b) => b,
                None => {
                    return Err(InvalidTraceError::no_sir(&loc.symbol_name));
                }
            };

            // Store trace inputs local and forward it to the TIR trace.
            trace_inputs_local = body.trace_inputs_local;

            // Initialise VarRenamer's accumulator (and thus also set the first offset) to the
            // traces most outer number of locals.
            rnm.init_acc(body.local_decls.len());

            // When adding statements to the trace, we clone them (rather than referencing the
            // statements in the SIR) so that we have the freedom to mutate them later.
            let user_bb_idx_usize = usize::try_from(loc.bb_idx).unwrap();
            // When converting the SIR trace into a TIR trace we alpha-rename the `Local`s from
            // inlined functions by adding an offset to each. This offset is derived from the
            // number of assigned variables in the functions outer context. For example, if a
            // function `bar` is inlined into a function `foo`, and `foo` used 5 variables, then
            // all variables in `bar` are offset by 5.
            for stmt in body.blocks[user_bb_idx_usize].stmts.iter() {
                let op = match stmt {
                    Statement::StorageLive(local) => {
                        let renamed = rnm.rename_local(local, body);
                        Statement::StorageLive(renamed)
                    }
                    Statement::StorageDead(local) => {
                        Statement::StorageDead(rnm.rename_local(local, body))
                    }
                    Statement::Assign(place, rvalue) => {
                        let newplace = rnm.rename_place(&place, body);
                        let newrvalue = rnm.rename_rvalue(&rvalue, body);
                        Statement::Assign(newplace, newrvalue)
                    }
                    Statement::Nop => stmt.clone(),
                    Statement::Unimplemented(_) => stmt.clone(),
                    // The following statements kinds are specific to TIR and cannot appear in SIR.
                    Statement::Call(..) | Statement::Enter(..) | Statement::Leave => unreachable!()
                };
                ops.push(TirOp::Statement(op));
            }

            match &body.blocks[user_bb_idx_usize].term {
                Terminator::Call {
                    operand: op,
                    args,
                    destination: dest
                } => {
                    // Rename the return value.
                    //
                    // FIXME It seems that calls always have a destination despite the field being
                    // `Option`. If this is not always the case, we may want add the `Local` offset
                    // (`var_len`) to this statement so we can assign the arguments to the correct
                    // `Local`s during trace compilation.
                    let ret_val = dest
                        .as_ref()
                        .map(|(ret_val, _)| rnm.rename_place(&ret_val, body))
                        .unwrap();

                    if let Some(callee_sym) = op.symbol() {
                        // We know the symbol name of the callee at least.
                        // Rename all `Local`s within the arguments.
                        let newargs = rnm.rename_args(&args, body);
                        let op = if let Some(callbody) = SIR.bodies.get(callee_sym) {
                            // We have SIR for the callee, so it will appear inlined in the trace
                            // and we only need to emit Enter/Leave statements.

                            // Inform VarRenamer about this function's offset, which is equal to the
                            // number of variables assigned in the outer body.
                            rnm.enter(callbody.local_decls.len(), ret_val.clone());

                            // Ensure the callee's arguments get TIR local decls. This is required
                            // because arguments are implicitly live at the start of each function,
                            // and we usually instantiate local decls when we see a StorageLive.
                            //
                            // This must happen after rnm.enter() so that self.offset is up-to-date.
                            for lidx in 0..newargs.len() {
                                let lidx = lidx + 1; // Skipping the return local.
                                let decl = &callbody.local_decls[usize::try_from(lidx).unwrap()];
                                rnm.used_decl(
                                    Local(rnm.offset + u32::try_from(lidx).unwrap()),
                                    decl.clone()
                                );
                            }

                            TirOp::Statement(Statement::Enter(
                                op.clone(),
                                newargs,
                                Some(ret_val),
                                rnm.offset()
                            ))
                        } else {
                            // We have a symbol name but no SIR. Without SIR the callee can't
                            // appear inlined in the trace, so we should emit a native call to the
                            // symbol instead.
                            TirOp::Statement(Statement::Call(op.clone(), newargs, Some(ret_val)))
                        };
                        ops.push(op);
                    } else {
                        todo!("Unknown callee encountered");
                    }
                }
                Terminator::Return => {
                    // After leaving an inlined function call we need to clean up any renaming
                    // mappings we have added manually, because we don't get `StorageDead`
                    // statements for call arguments. Which mappings we need to remove depends on
                    // the number of arguments the function call had, which we keep track of in
                    // `cur_call_args`.
                    rnm.leave();
                    ops.push(TirOp::Statement(Statement::Leave))
                }
                _ => {}
            }

            // Convert the block terminator to a guard if necessary.
            let guard = match body.blocks[user_bb_idx_usize].term {
                Terminator::Goto(_)
                | Terminator::Return
                | Terminator::Drop { .. }
                | Terminator::DropAndReplace { .. }
                | Terminator::Call { .. }
                | Terminator::Unimplemented(_) => None,
                Terminator::Unreachable => panic!("Traced unreachable code"),
                Terminator::SwitchInt {
                    ref discr,
                    ref values,
                    ref target_bbs,
                    otherwise_bb
                } => {
                    // Peek at the next block in the trace to see which outgoing edge was taken and
                    // infer which value we must guard upon. We are working on the assumption that
                    // a trace can't end on a SwitchInt. i.e. that another block follows.
                    let next_blk = itr.peek().expect("no block to peek at").bb_idx;
                    let edge_idx = target_bbs.iter().position(|e| *e == next_blk);
                    match edge_idx {
                        Some(idx) => Some(Guard {
                            val: discr.clone(),
                            kind: GuardKind::Integer(values[idx].val())
                        }),
                        None => {
                            debug_assert!(next_blk == otherwise_bb);
                            Some(Guard {
                                val: discr.clone(),
                                kind: GuardKind::OtherInteger(
                                    values.iter().map(|v| v.val()).collect()
                                )
                            })
                        }
                    }
                }
                Terminator::Assert {
                    ref cond,
                    ref expected,
                    ..
                } => Some(Guard {
                    val: cond.clone(),
                    kind: GuardKind::Boolean(*expected)
                })
            };

            if guard.is_some() {
                ops.push(TirOp::Guard(guard.unwrap()));
            }
        }

        // Remove the remnants of the `start_tracing` (1 instruction) and `stop_tracing` (5
        // instructions) calls at the beginning and end of the trace.  To be sure we don't remove
        // something important, let's put some asserts in to check these instructions are always
        // roughly the same.
        match ops.pop() {
            Some(TirOp::Statement(Statement::Enter(CallOperand::Fn(s), _, _, _))) => {
                debug_assert!(s.contains("stop_tracing"))
            }
            e => panic!("Expected call to `stop_tracing` here, instead got {:?}.", e)
        }
        match ops.pop() {
            Some(TirOp::Statement(Statement::Assign(_, _))) => {}
            e => panic!("Expected `Assign` here, instead got {:?}.", e)
        }
        match ops.pop() {
            Some(TirOp::Statement(Statement::Assign(
                _,
                Rvalue::Use(Operand::Constant(Constant::Bool(false)))
            ))) => {}
            e => panic!("Expected `Assign(_, false)` here, instead got {:?}.", e)
        }
        for _ in 0..3 {
            match ops.pop() {
                Some(TirOp::Statement(Statement::StorageLive(_))) => {}
                e => panic!("Expected `StorageLive` here, instead got {:?}.", e)
            }
        }
        match ops.remove(0) {
            TirOp::Statement(Statement::StorageDead(_)) => {}
            e => panic!("Expected `StorageDead` here, instead got {:?}.", e)
        }

        Ok(Self {
            ops,
            trace_inputs_local,
            local_decls: rnm.done()
        })
    }

    /// Return the TIR operation at index `idx` in the trace.
    /// The index must not be out of bounds.
    pub fn op(&self, idx: usize) -> &TirOp {
        debug_assert!(idx <= self.ops.len() - 1, "bogus trace index");
        unsafe { &self.ops.get_unchecked(idx) }
    }

    pub fn inputs(&self) -> &Option<Local> {
        &self.trace_inputs_local
    }

    /// Return the length of the trace measure in operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

struct VarRenamer {
    /// Stores the offset before entering an inlined call, so that the correct offset can be
    /// restored again after leaving that call.
    stack: Vec<u32>,
    /// Current offset used to rename variables.
    offset: u32,
    /// Accumulator keeping track of total number of variables used. Needed to use different
    /// offsets for consecutive inlined function calls.
    acc: Option<u32>,
    /// Stores the return variables of inlined function calls. Used to replace `$0` during
    /// renaming.
    returns: Vec<Place>,
    /// Used local declarations.
    /// Used to keep track of only the local declarations that are actually used in the trace.
    ///
    /// FIXME Hopefully in the future there will be a better mechanism for finding the used locals
    /// in a trace. We had planned to use `StorageLive` as a mechanism to identify them, but sadly
    /// temporary variables and variables in cleanup code are never marked live (they are assumed
    /// to live the whole function).
    used_decls: HashMap<Local, LocalDecl>
}

impl VarRenamer {
    fn new() -> Self {
        VarRenamer {
            stack: vec![0],
            offset: 0,
            acc: None,
            returns: Vec::new(),
            used_decls: HashMap::new()
        }
    }

    /// Register a used local declaration.
    fn used_decl(&mut self, l: Local, decl: LocalDecl) {
        self.used_decls.insert(l, decl);
    }

    /// Finalises the renamer, returning the local decls for the trace.
    fn done(self) -> HashMap<Local, LocalDecl> {
        self.used_decls
    }

    fn offset(&self) -> u32 {
        self.offset
    }

    fn init_acc(&mut self, num_locals: usize) {
        if self.acc.is_none() {
            self.acc.replace(num_locals as u32);
        }
    }

    fn enter(&mut self, num_locals: usize, dest: Place) {
        // When entering an inlined function call set the offset to the current accumulator. Then
        // increment the accumulator by the number of locals in the current function. Also add the
        // offset to the stack, so we can restore it once we leave the inlined function call again.
        self.offset = self.acc.unwrap();
        self.stack.push(self.offset);
        match self.acc.as_mut() {
            Some(v) => *v += num_locals as u32,
            None => {}
        }
        self.returns.push(dest);
    }

    fn leave(&mut self) {
        // When we leave an inlined function call, we pop the previous offset from the stack,
        // reverting the offset to what it was before the function was entered.
        self.stack.pop();
        self.returns.pop();
        if let Some(v) = self.stack.last() {
            self.offset = *v;
        } else {
            panic!("Unbalanced enter/leave statements!")
        }
    }

    fn rename_args(&mut self, args: &Vec<Operand>, body: &ykpack::Body) -> Vec<Operand> {
        args.iter()
            .map(|op| self.rename_operand(&op, body))
            .collect()
    }

    fn rename_rvalue(&mut self, rvalue: &Rvalue, body: &ykpack::Body) -> Rvalue {
        match rvalue {
            Rvalue::Use(op) => {
                let newop = self.rename_operand(op, body);
                Rvalue::Use(newop)
            }
            Rvalue::BinaryOp(binop, op1, op2) => {
                let newop1 = self.rename_operand(op1, body);
                let newop2 = self.rename_operand(op2, body);
                Rvalue::BinaryOp(binop.clone(), newop1, newop2)
            }
            Rvalue::CheckedBinaryOp(binop, op1, op2) => {
                let newop1 = self.rename_operand(op1, body);
                let newop2 = self.rename_operand(op2, body);
                Rvalue::CheckedBinaryOp(binop.clone(), newop1, newop2)
            }
            Rvalue::Ref(place) => {
                let newplace = self.rename_place(place, body);
                Rvalue::Ref(newplace)
            }
            Rvalue::Unimplemented(_) => rvalue.clone()
        }
    }

    fn rename_operand(&mut self, operand: &Operand, body: &ykpack::Body) -> Operand {
        match operand {
            Operand::Place(p) => Operand::Place(self.rename_place(p, body)),
            Operand::Constant(_) => operand.clone()
        }
    }

    fn rename_place(&mut self, place: &Place, body: &ykpack::Body) -> Place {
        if &place.local == &Local(0) {
            // Replace the default return variable $0 with the variable in the outer context where
            // the return value will end up after leaving the function. This saves us an
            // instruction when we compile the trace.
            if let Some(v) = self.returns.last() {
                v.clone()
            } else {
                panic!("Expected return value!")
            }
        } else {
            let mut p = place.clone();
            p.local = self.rename_local(&p.local, body);
            p
        }
    }

    fn rename_local(&mut self, local: &Local, body: &ykpack::Body) -> Local {
        let renamed = Local(local.0 + self.offset);
        self.used_decl(
            renamed.clone(),
            body.local_decls[usize::try_from(local.0).unwrap()].clone()
        );
        renamed
    }
}

impl Display for TirTrace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "local_decls:")?;
        let mut sort_decls = self
            .local_decls
            .iter()
            .collect::<Vec<(&Local, &LocalDecl)>>();
        sort_decls.sort_by(|l, r| l.0.partial_cmp(r.0).unwrap());
        for (l, dcl) in sort_decls {
            writeln!(
                f,
                "  {}: ({}, {}) => {}",
                l,
                dcl.ty.0,
                dcl.ty.1,
                SIR.ty(&dcl.ty)
            )?;
        }

        writeln!(f, "ops:")?;
        for op in &self.ops {
            writeln!(f, "  {}", op)?;
        }
        Ok(())
    }
}

/// A guard states the assumptions from its position in a trace onward.
#[derive(Debug)]
pub struct Guard {
    /// The value to be checked if the guard is to pass.
    pub val: Place,
    /// The requirement upon `val` for the guard to pass.
    pub kind: GuardKind
}

/// A guard states the assumptions from its position in a trace onward.
#[derive(Debug)]
pub enum GuardKind {
    /// The value must be equal to an integer constant.
    Integer(u128),
    /// The value must not be a member of the specified collection of integers. This is necessary
    /// due to the "otherwise" semantics of the `SwitchInt` terminator in SIR.
    OtherInteger(Vec<u128>),
    /// The value must equal a Boolean constant.
    Boolean(bool)
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "guard({}, {})", self.val, self.kind)
    }
}

impl fmt::Display for GuardKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Integer(u128v) => write!(f, "integer({})", u128v),
            Self::OtherInteger(u128vs) => write!(f, "other_integer({:?})", u128vs),
            Self::Boolean(expect) => write!(f, "bool({})", expect)
        }
    }
}

/// A TIR operation. A collection of these makes a TIR trace.
#[derive(Debug)]
pub enum TirOp {
    Statement(Statement),
    Guard(Guard)
}

impl fmt::Display for TirOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TirOp::Statement(st) => write!(f, "{}", st),
            TirOp::Guard(gd) => write!(f, "{}", gd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TirTrace;
    use crate::{start_tracing, TracingKind};
    use test::black_box;

    // Some work to trace.
    #[inline(never)]
    fn work(x: usize, y: usize) -> usize {
        let mut res = 0;
        while res < y {
            res += x;
        }
        res
    }

    #[test]
    fn nonempty_tir_trace() {
        #[cfg(tracermode = "sw")]
        let tracer = start_tracing(Some(TracingKind::SoftwareTracing));
        #[cfg(tracermode = "hw")]
        let tracer = start_tracing(Some(TracingKind::HardwareTracing));

        let res = black_box(work(black_box(3), black_box(13)));
        let sir_trace = tracer.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*sir_trace).unwrap();
        assert_eq!(res, 15);
        assert!(tir_trace.len() > 0);
    }
}
