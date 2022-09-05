use crate::{
    debug::{DebugArena, DebugNode, DebugStep, Instruction},
    executor::{
        backend::DatabaseExt,
        inspector::utils::{gas_used, get_create_address},
        CHEATCODE_ADDRESS,
    },
    CallKind,
};
use bytes::Bytes;
use ethers::types::Address;
use revm::{
    opcode, spec_opcode_gas, CallInputs, CreateInputs, EVMData, Gas, Inspector, Interpreter,
    Memory, Return,
};

/// An inspector that collects debug nodes on every step of the interpreter.
#[derive(Default, Debug)]
pub struct Debugger {
    /// The arena of [DebugNode]s
    pub arena: DebugArena,
    /// The ID of the current [DebugNode].
    pub head: usize,
    /// The current execution address.
    pub context: Address,
    /// The amount of gas spent in the current gas block.
    ///
    /// REVM adds gas in blocks, so we need to keep track of this separately to get accurate gas
    /// numbers on an opcode level.
    ///
    /// Gas blocks contain the gas costs of opcodes with a fixed cost. Dynamic costs are not
    /// included in the gas block, and are instead added during execution of the contract.
    pub current_gas_block: u64,
    /// The amount of gas spent in the previous gas block.
    ///
    /// Costs for gas blocks are accounted for when *entering* the gas block, which also means that
    /// every run of the interpreter will always start with a non-zero `gas.spend()`.
    ///
    /// For more information on gas blocks, see [current_gas_block].
    pub previous_gas_block: u64,
}

impl Debugger {
    /// Enters a new execution context.
    pub fn enter(&mut self, depth: usize, address: Address, kind: CallKind) {
        self.context = address;
        self.head = self.arena.push_node(DebugNode { depth, address, kind, ..Default::default() });
    }

    /// Exits the current execution context, replacing it with the previous one.
    pub fn exit(&mut self) {
        if let Some(parent_id) = self.arena.arena[self.head].parent {
            let DebugNode { depth, address, kind, .. } = self.arena.arena[parent_id];
            self.context = address;
            self.head =
                self.arena.push_node(DebugNode { depth, address, kind, ..Default::default() });
        }
    }
}

impl<DB> Inspector<DB> for Debugger
where
    DB: DatabaseExt,
{
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter,
        _: &mut EVMData<'_, DB>,
        _: bool,
    ) -> Return {
        self.previous_gas_block = interp.contract.first_gas_block();
        Return::Continue
    }

    fn step(
        &mut self,
        interpreter: &mut Interpreter,
        data: &mut EVMData<'_, DB>,
        _is_static: bool,
    ) -> Return {
        let pc = interpreter.program_counter();
        let op = interpreter.contract.bytecode.bytecode()[pc];

        // Get opcode information
        let opcode_infos = spec_opcode_gas(data.env.cfg.spec_id);
        let opcode_info = &opcode_infos[op as usize];

        // Extract the push bytes
        let push_size = if opcode_info.is_push() { (op - opcode::PUSH1 + 1) as usize } else { 0 };
        let push_bytes = match push_size {
            0 => None,
            n => {
                let start = pc + 1;
                let end = start + n;
                Some(interpreter.contract.bytecode.bytecode()[start..end].to_vec())
            }
        };

        // Calculate the current amount of gas used
        let gas = interpreter.gas();
        let total_gas_spent = gas
            .spend()
            .saturating_sub(self.previous_gas_block)
            .saturating_add(self.current_gas_block);
        if opcode_info.is_gas_block_end() {
            self.previous_gas_block = interpreter.contract.gas_block(pc);
            self.current_gas_block = 0;
        } else {
            self.current_gas_block += opcode_info.get_gas() as u64;
        }

        self.arena.arena[self.head].steps.push(DebugStep {
            pc,
            stack: interpreter.stack().data().clone(),
            memory: interpreter.memory.clone(),
            instruction: Instruction::OpCode(op),
            push_bytes,
            total_gas_used: gas_used(data.env.cfg.spec_id, total_gas_spent, gas.refunded() as u64),
        });

        Return::Continue
    }

    fn call(
        &mut self,
        data: &mut EVMData<'_, DB>,
        call: &mut CallInputs,
        _: bool,
    ) -> (Return, Gas, Bytes) {
        self.enter(
            data.journaled_state.depth() as usize,
            call.context.code_address,
            call.context.scheme.into(),
        );
        if call.contract == CHEATCODE_ADDRESS {
            self.arena.arena[self.head].steps.push(DebugStep {
                memory: Memory::new(),
                instruction: Instruction::Cheatcode(
                    call.input[0..4].try_into().expect("malformed cheatcode call"),
                ),
                ..Default::default()
            });
        }

        (Return::Continue, Gas::new(call.gas_limit), Bytes::new())
    }

    fn call_end(
        &mut self,
        _: &mut EVMData<'_, DB>,
        _: &CallInputs,
        gas: Gas,
        status: Return,
        retdata: Bytes,
        _: bool,
    ) -> (Return, Gas, Bytes) {
        self.exit();

        (status, gas, retdata)
    }

    fn create(
        &mut self,
        data: &mut EVMData<'_, DB>,
        call: &mut CreateInputs,
    ) -> (Return, Option<Address>, Gas, Bytes) {
        // TODO: Does this increase gas cost?
        if let Err(err) = data.journaled_state.load_account(call.caller, data.db) {
            return (Return::Revert, None, Gas::new(call.gas_limit), err.string_encoded())
        }

        let nonce = data.journaled_state.account(call.caller).info.nonce;
        self.enter(
            data.journaled_state.depth() as usize,
            get_create_address(call, nonce),
            CallKind::Create,
        );

        (Return::Continue, None, Gas::new(call.gas_limit), Bytes::new())
    }

    fn create_end(
        &mut self,
        _: &mut EVMData<'_, DB>,
        _: &CreateInputs,
        status: Return,
        address: Option<Address>,
        gas: Gas,
        retdata: Bytes,
    ) -> (Return, Option<Address>, Gas, Bytes) {
        self.exit();

        (status, address, gas, retdata)
    }
}
