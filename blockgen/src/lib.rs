// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use cfx_types::{Address, H256, U256};
use cfxcore::{
    block_parameters::*,
    miner::{
        stratum::{Options as StratumOption, Stratum},
        work_notify::NotifyWork,
    },
    parameters::consensus::GENESIS_GAS_LIMIT,
    pow::*,
    ConsensusGraph, ConsensusGraphTrait, SharedSynchronizationGraph,
    SharedSynchronizationService, SharedTransactionPool, Stopable,
};
use lazy_static::lazy_static;
use log::{debug, trace, warn};
use metrics::{Gauge, GaugeUsize};
use parking_lot::{Mutex, RwLock};
use primitives::*;
use std::{
    cmp::max,
    collections::HashSet,
    sync::{mpsc, Arc},
    thread, time,
};
use time::{Duration, SystemTime, UNIX_EPOCH};
use txgen::SharedTransactionGenerator;
lazy_static! {
    static ref PACKED_ACCOUNT_SIZE: Arc<dyn Gauge<usize>> =
        GaugeUsize::register_with_group("txpool", "packed_account_size");
}

enum MiningState {
    Start,
    Stop,
}

/// The interface for a conflux block generator
pub struct BlockGenerator {
    pub pow_config: ProofOfWorkConfig,
    mining_author: Address,
    graph: SharedSynchronizationGraph,
    txpool: SharedTransactionPool,
    maybe_txgen: Option<SharedTransactionGenerator>,
    sync: SharedSynchronizationService,
    state: RwLock<MiningState>,
    workers: Mutex<Vec<(Worker, mpsc::Sender<ProofOfWorkProblem>)>>,
    pub stratum: RwLock<Option<Stratum>>,
}

pub struct Worker {
    #[allow(dead_code)]
    thread: thread::JoinHandle<()>,
}

impl Worker {
    pub fn new(
        bg: Arc<BlockGenerator>,
        solution_sender: mpsc::Sender<ProofOfWorkSolution>,
        problem_receiver: mpsc::Receiver<ProofOfWorkProblem>,
    ) -> Self
    {
        let bg_handle = bg;

        let thread = thread::Builder::new()
            .name("blockgen".into())
            .spawn(move || {
                let sleep_duration = time::Duration::from_millis(100);
                let mut problem: Option<ProofOfWorkProblem> = None;

                loop {
                    match *bg_handle.state.read() {
                        MiningState::Stop => return,
                        _ => {}
                    }

                    // check if there is a new problem
                    let new_problem = problem_receiver.try_recv();
                    if new_problem.is_ok() {
                        problem = Some(new_problem.unwrap());
                    }
                    // check if there is a problem to be solved
                    if problem.is_some() {
                        let boundary = problem.as_ref().unwrap().boundary;
                        let block_hash = problem.as_ref().unwrap().block_hash;

                        for _i in 0..100_000 {
                            //TODO: adjust the number of times
                            let nonce = rand::random();
                            let hash = compute(nonce, &block_hash);
                            if ProofOfWorkProblem::validate_hash_against_boundary(&hash, &boundary) {
                                // problem solved
                                match solution_sender
                                    .send(ProofOfWorkSolution { nonce })
                                {
                                    Ok(_) => {}
                                    Err(e) => {
                                        warn!("{}", e);
                                    }
                                }
                                // TODO Update problem fast. This will cause
                                // miner to stop mining
                                // until the previous blocks is processed by
                                // ConsensusGraph
                                problem = None;
                                break;
                            }
                        }
                    } else {
                        thread::sleep(sleep_duration);
                    }
                }
            })
            .expect("only one blockgen thread, so it should not fail");
        Worker { thread }
    }
}

impl BlockGenerator {
    pub fn new(
        graph: SharedSynchronizationGraph, txpool: SharedTransactionPool,
        sync: SharedSynchronizationService,
        maybe_txgen: Option<SharedTransactionGenerator>,
        pow_config: ProofOfWorkConfig, mining_author: Address,
    ) -> Self
    {
        BlockGenerator {
            pow_config,
            mining_author,
            graph,
            txpool,
            maybe_txgen,
            sync,
            state: RwLock::new(MiningState::Start),
            workers: Mutex::new(Vec::new()),
            stratum: RwLock::new(None),
        }
    }

    /// Stop mining
    pub fn stop(&self) {
        {
            let mut write = self.state.write();
            *write = MiningState::Stop;
        }
        if let Some(txgen) = self.maybe_txgen.as_ref() {
            txgen.stop()
        }
    }

    /// Send new PoW problem to workers
    pub fn send_problem(bg: Arc<BlockGenerator>, problem: ProofOfWorkProblem) {
        if bg.pow_config.use_stratum {
            let stratum = bg.stratum.read();
            stratum
                .as_ref()
                .unwrap()
                .notify(problem.block_hash, problem.boundary);
        } else {
            for item in bg.workers.lock().iter() {
                item.1
                    .send(problem)
                    .expect("Failed to send the PoW problem.")
            }
        }
    }

    // TODO: should not hold and pass write lock to consensus.
    fn assemble_new_block_impl(
        &self, parent_hash: H256, mut referees: Vec<H256>, blame: u32,
        deferred_state_root: H256, deferred_receipts_root: H256,
        deferred_logs_bloom_hash: H256, block_gas_limit: U256,
        transactions: Vec<Arc<SignedTransaction>>, difficulty: u64,
        adaptive_opt: Option<bool>,
    ) -> Block
    {
        let parent_height =
            self.graph.block_height_by_hash(&parent_hash).unwrap();

        let parent_timestamp =
            self.graph.block_timestamp_by_hash(&parent_hash).unwrap();

        trace!("{} txs packed", transactions.len());
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");

        let mut consensus_inner = consensus_graph.inner.write();
        // referees are retrieved before locking inner, so we need to
        // filter out the blocks that should be removed by possible
        // checkpoint making that happens before we acquire the inner lock
        referees
            .retain(|h| consensus_inner.hash_to_arena_indices.contains_key(h));
        let mut expected_difficulty =
            consensus_inner.expected_difficulty(&parent_hash);
        let adaptive = if let Some(x) = adaptive_opt {
            x
        } else {
            consensus_graph.check_mining_adaptive_block(
                &mut *consensus_inner,
                &parent_hash,
                &referees,
                &expected_difficulty,
            )
        };

        if U256::from(difficulty) > expected_difficulty {
            expected_difficulty = U256::from(difficulty);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Adjust the timestamp of the currently mined block to be later
        // than or equal to its parent's.
        // See comments in verify_header_graph_ready_block()
        let my_timestamp = max(parent_timestamp, now);

        let block_header = BlockHeaderBuilder::new()
            .with_transactions_root(Block::compute_transaction_root(
                &transactions,
            ))
            .with_parent_hash(parent_hash)
            .with_height(parent_height + 1)
            .with_timestamp(my_timestamp)
            .with_author(self.mining_author)
            .with_blame(blame)
            .with_deferred_state_root(deferred_state_root)
            .with_deferred_receipts_root(deferred_receipts_root)
            .with_deferred_logs_bloom_hash(deferred_logs_bloom_hash)
            .with_difficulty(expected_difficulty)
            .with_adaptive(adaptive)
            .with_referee_hashes(referees)
            .with_nonce(0)
            .with_gas_limit(block_gas_limit)
            .build();

        Block::new(block_header, transactions)
    }

    /// Assemble a new block with specified parent and referee, this is for test
    /// only
    pub fn assemble_new_fixed_block(
        &self, parent_hash: H256, referee: Vec<H256>, num_txs: usize,
        difficulty: u64, adaptive: bool, block_gas_limit: u64,
    ) -> Result<Block, String>
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");
        let (blame, state_root, receipts_root, logs_bloom_hash) =
            consensus_graph
                .force_compute_blame_and_deferred_state_for_generation(
                    &parent_hash,
                )?;

        let block_gas_limit = block_gas_limit.into();
        let block_size_limit =
            self.graph.verification_config.max_block_size_in_bytes;

        let transactions = self.txpool.pack_transactions(
            num_txs,
            block_gas_limit,
            block_size_limit,
            consensus_graph.best_epoch_number(),
        );

        Ok(self.assemble_new_block_impl(
            parent_hash,
            referee,
            blame,
            state_root,
            receipts_root,
            logs_bloom_hash,
            block_gas_limit,
            transactions,
            difficulty,
            Some(adaptive),
        ))
    }

    /// Assemble a new block without nonce
    pub fn assemble_new_block(
        &self, num_txs: usize, block_size_limit: usize,
        additional_transactions: Vec<Arc<SignedTransaction>>,
    ) -> Block
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");

        let (best_info, block_gas_limit, transactions) =
            self.txpool.get_best_info_with_packed_transactions(
                num_txs,
                block_size_limit,
                additional_transactions,
            );

        let mut sender_accounts = HashSet::new();
        for tx in &transactions {
            let tx_hash = tx.hash();
            if tx_hash[0] & 254 == 0 {
                debug!("Sampled transaction {:?} in packing block", tx_hash);
            }
            sender_accounts.insert(tx.sender);
        }
        PACKED_ACCOUNT_SIZE.update(sender_accounts.len());

        let (
            blame,
            deferred_state_root,
            deferred_receipts_root,
            deferred_logs_bloom_hash,
        ) = consensus_graph
            .get_blame_and_deferred_state_for_generation(
                &best_info.best_block_hash,
            )
            .unwrap();

        let best_block_hash = best_info.best_block_hash.clone();
        let mut referee = best_info.bounded_terminal_block_hashes.clone();
        referee.retain(|r| *r != best_block_hash);

        self.assemble_new_block_impl(
            best_block_hash,
            referee,
            blame,
            deferred_state_root,
            deferred_receipts_root,
            deferred_logs_bloom_hash,
            block_gas_limit,
            transactions,
            0,
            None,
        )
    }

    /// Assemble a new block without nonce and with options to override the
    /// states/blame. This function is used for testing only to generate
    /// incorrect blocks
    pub fn assemble_new_block_with_blame_info(
        &self, num_txs: usize, block_size_limit: usize,
        additional_transactions: Vec<Arc<SignedTransaction>>,
        blame_override: Option<u32>, state_root_override: Option<H256>,
        receipt_root_override: Option<H256>,
        logs_bloom_hash_override: Option<H256>,
    ) -> Block
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");

        let (best_info, block_gas_limit, transactions) =
            self.txpool.get_best_info_with_packed_transactions(
                num_txs,
                block_size_limit,
                additional_transactions,
            );

        let (
            mut blame,
            mut deferred_state_root,
            mut deferred_receipts_root,
            mut deferred_logs_bloom_hash,
        ) = consensus_graph
            .get_blame_and_deferred_state_for_generation(
                &best_info.best_block_hash,
            )
            .unwrap();

        if let Some(x) = blame_override {
            blame = x;
        }
        if let Some(x) = state_root_override {
            deferred_state_root = x;
        }
        if let Some(x) = receipt_root_override {
            deferred_receipts_root = x;
        }
        if let Some(x) = logs_bloom_hash_override {
            deferred_logs_bloom_hash = x;
        }

        let best_block_hash = best_info.best_block_hash.clone();
        let mut referee = best_info.bounded_terminal_block_hashes.clone();
        referee.retain(|r| *r != best_block_hash);

        self.assemble_new_block_impl(
            best_block_hash,
            referee,
            blame,
            deferred_state_root,
            deferred_receipts_root,
            deferred_logs_bloom_hash,
            block_gas_limit,
            transactions,
            0,
            None,
        )
    }

    /// Update and sync a new block
    pub fn on_mined_block(&self, block: Block) {
        self.sync.on_mined_block(block);
    }

    /// Check if we need to mine on a new block
    pub fn is_mining_block_outdated(&self, block: Option<&Block>) -> bool {
        if block.is_none() {
            return true;
        }

        // 1st Check: if the parent block changed
        let best_block_hash = self.graph.consensus.best_block_hash();
        if best_block_hash != *block.unwrap().block_header.parent_hash() {
            return true;
        }
        // TODO: 2nd check: if the referee hashes changed
        // TODO: 3rd check: if we want to pack a new set of transactions
        false
    }

    // This function is used in test only to simulate attacker behavior.
    pub fn generate_fixed_block(
        &self, parent_hash: H256, referee: Vec<H256>, num_txs: usize,
        difficulty: u64, adaptive: bool,
    ) -> Result<H256, String>
    {
        let block = self.assemble_new_fixed_block(
            parent_hash,
            referee,
            num_txs,
            difficulty,
            adaptive,
            GENESIS_GAS_LIMIT,
        )?;
        Ok(self.generate_block_impl(block))
    }

    /// Generate a block with transactions in the pool
    pub fn generate_block(
        &self, num_txs: usize, block_size_limit: usize,
        additional_transactions: Vec<Arc<SignedTransaction>>,
    ) -> H256
    {
        let block = self.assemble_new_block(
            num_txs,
            block_size_limit,
            additional_transactions,
        );
        self.generate_block_impl(block)
    }

    /// Generate a block with transactions in the pool.
    /// This is used for testing only
    pub fn generate_block_with_blame_info(
        &self, num_txs: usize, block_size_limit: usize,
        additional_transactions: Vec<Arc<SignedTransaction>>,
        blame: Option<u32>, state_root: Option<H256>,
        receipts_root: Option<H256>, logs_bloom_hash: Option<H256>,
    ) -> H256
    {
        let block = self.assemble_new_block_with_blame_info(
            num_txs,
            block_size_limit,
            additional_transactions,
            blame,
            state_root,
            receipts_root,
            logs_bloom_hash,
        );
        self.generate_block_impl(block)
    }

    pub fn generate_custom_block(
        &self, transactions: Vec<Arc<SignedTransaction>>,
        adaptive: Option<bool>,
    ) -> H256
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");
        // get the best block
        let (best_info, block_gas_limit, _) = self
            .txpool
            .get_best_info_with_packed_transactions(0, 0, Vec::new());
        let (
            blame,
            deferred_state_root,
            deferred_receipts_root,
            deferred_logs_bloom_hash,
        ) = consensus_graph
            .get_blame_and_deferred_state_for_generation(
                &best_info.best_block_hash,
            )
            .unwrap();

        let best_block_hash = best_info.best_block_hash.clone();
        let mut referee = best_info.bounded_terminal_block_hashes.clone();
        referee.retain(|r| *r != best_block_hash);

        let block = self.assemble_new_block_impl(
            best_block_hash,
            referee,
            blame,
            deferred_state_root,
            deferred_receipts_root,
            deferred_logs_bloom_hash,
            block_gas_limit,
            transactions,
            0,
            adaptive,
        );

        self.generate_block_impl(block)
    }

    pub fn generate_custom_block_with_parent(
        &self, parent_hash: H256, referee: Vec<H256>,
        transactions: Vec<Arc<SignedTransaction>>, adaptive: bool,
    ) -> Result<H256, String>
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");
        let (blame, state_root, receipts_root, logs_bloom_hash) =
            consensus_graph
                .force_compute_blame_and_deferred_state_for_generation(
                    &parent_hash,
                )?;

        let block = self.assemble_new_block_impl(
            parent_hash,
            referee,
            blame,
            state_root,
            receipts_root,
            logs_bloom_hash,
            GENESIS_GAS_LIMIT.into(),
            transactions,
            0,
            Some(adaptive),
        );

        Ok(self.generate_block_impl(block))
    }

    pub fn generate_block_with_nonce_and_timestamp(
        &self, parent_hash: H256, referee: Vec<H256>,
        transactions: Vec<Arc<SignedTransaction>>, nonce: u64, timestamp: u64,
        adaptive: bool,
    ) -> Result<H256, String>
    {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");
        let (blame, state_root, receipts_root, logs_bloom_hash) =
            consensus_graph
                .force_compute_blame_and_deferred_state_for_generation(
                    &parent_hash,
                )?;

        let mut block = self.assemble_new_block_impl(
            parent_hash,
            referee,
            blame,
            state_root,
            receipts_root,
            logs_bloom_hash,
            GENESIS_GAS_LIMIT.into(),
            transactions,
            0,
            Some(adaptive),
        );
        block.block_header.set_nonce(nonce);
        block.block_header.set_timestamp(timestamp);

        let hash = block.block_header.compute_hash();
        debug!(
            "generate_block with block header:{:?} tx_number:{}, block_size:{}",
            block.block_header,
            block.transactions.len(),
            block.size(),
        );
        self.on_mined_block(block);

        consensus_graph.wait_for_generation(&hash);
        Ok(hash)
    }

    fn generate_block_impl(&self, block_init: Block) -> H256 {
        let consensus_graph = self
            .graph
            .consensus
            .as_any()
            .downcast_ref::<ConsensusGraph>()
            .expect("downcast should succeed");
        let mut block = block_init;
        let difficulty = block.block_header.difficulty();
        let problem = ProofOfWorkProblem::new(
            block.block_header.problem_hash(),
            *difficulty,
        );
        loop {
            let nonce = rand::random();
            if validate(&problem, &ProofOfWorkSolution { nonce }) {
                block.block_header.set_nonce(nonce);
                break;
            }
        }
        let hash = block.block_header.compute_hash();
        debug!(
            "generate_block with block header:{:?} tx_number:{}, block_size:{}",
            block.block_header,
            block.transactions.len(),
            block.size(),
        );
        self.on_mined_block(block);

        // FIXME: We should add a flag to enable/disable this wait
        // Ensure that when `generate**` function returns, the block has been
        // handled by Consensus This order is assumed by some tests, and
        // this function is also only used in tests.
        consensus_graph.wait_for_generation(&hash);

        hash
    }

    pub fn pow_config(&self) -> ProofOfWorkConfig { self.pow_config.clone() }

    /// Start num_worker new workers
    pub fn start_new_worker(
        num_worker: u32, bg: Arc<BlockGenerator>,
    ) -> mpsc::Receiver<ProofOfWorkSolution> {
        let (solution_sender, solution_receiver) = mpsc::channel();
        let mut workers = bg.workers.lock();
        for _ in 0..num_worker {
            let (problem_sender, problem_receiver) = mpsc::channel();
            workers.push((
                Worker::new(
                    bg.clone(),
                    solution_sender.clone(),
                    problem_receiver,
                ),
                problem_sender,
            ));
        }
        solution_receiver
    }

    pub fn start_new_stratum_worker(
        bg: Arc<BlockGenerator>,
    ) -> mpsc::Receiver<ProofOfWorkSolution> {
        let (solution_sender, solution_receiver) = mpsc::channel();
        let cfg = StratumOption {
            listen_addr: bg.pow_config.stratum_listen_addr.clone(),
            port: bg.pow_config.stratum_port,
            secret: bg.pow_config.stratum_secret,
        };
        let stratum = Stratum::start(&cfg, solution_sender)
            .expect("Failed to start Stratum service.");
        let mut bg_stratum = bg.stratum.write();
        *bg_stratum = Some(stratum);
        solution_receiver
    }

    pub fn start_mining(bg: Arc<BlockGenerator>, _payload_len: u32) {
        let mut current_mining_block = None;
        let mut current_problem: Option<ProofOfWorkProblem> = None;
        // FIXME: change to notification.
        let sleep_duration = time::Duration::from_millis(50);

        let receiver: mpsc::Receiver<ProofOfWorkSolution> =
            if bg.pow_config.use_stratum {
                BlockGenerator::start_new_stratum_worker(bg.clone())
            } else {
                BlockGenerator::start_new_worker(1, bg.clone())
            };

        loop {
            match *bg.state.read() {
                MiningState::Stop => return,
                _ => {}
            }

            if bg.is_mining_block_outdated(current_mining_block.as_ref()) {
                // TODO: #transations TBD
                if !bg.pow_config.test_mode && bg.sync.catch_up_mode() {
                    thread::sleep(sleep_duration);
                    continue;
                }

                current_mining_block = Some(bg.assemble_new_block(
                    MAX_TRANSACTION_COUNT_PER_BLOCK,
                    bg.graph.verification_config.max_block_size_in_bytes,
                    vec![],
                ));

                // set a mining problem
                let current_difficulty = current_mining_block
                    .as_ref()
                    .unwrap()
                    .block_header
                    .difficulty();
                let problem = ProofOfWorkProblem::new(
                    current_mining_block
                        .as_ref()
                        .unwrap()
                        .block_header
                        .problem_hash(),
                    *current_difficulty,
                );
                BlockGenerator::send_problem(bg.clone(), problem);
                current_problem = Some(problem);
            } else {
                // check if the problem solved
                let mut new_solution = receiver.try_recv();
                loop {
                    // check if the block received valid
                    if new_solution.is_ok()
                        && !validate(
                            &current_problem.unwrap(),
                            &new_solution.unwrap(),
                        )
                    {
                        new_solution = receiver.try_recv();
                    } else {
                        break;
                    }
                }
                if new_solution.is_ok() {
                    let solution = new_solution.unwrap();
                    current_mining_block
                        .as_mut()
                        .unwrap()
                        .block_header
                        .set_nonce(solution.nonce);
                    current_mining_block
                        .as_mut()
                        .unwrap()
                        .block_header
                        .compute_hash();
                    bg.on_mined_block(current_mining_block.unwrap());
                    current_mining_block = None;
                    current_problem = None;
                } else {
                    // wait a moment and check again
                    thread::sleep(sleep_duration);
                    continue;
                }
            }
        }
    }

    pub fn auto_block_generation(&self, interval_ms: u64) {
        let interval = Duration::from_millis(interval_ms);
        loop {
            match *self.state.read() {
                MiningState::Stop => return,
                _ => {}
            }
            if !self.sync.catch_up_mode() {
                self.generate_block(
                    3000,
                    self.graph.verification_config.max_block_size_in_bytes,
                    vec![],
                );
            }
            thread::sleep(interval);
        }
    }
}

impl Stopable for BlockGenerator {
    fn stop(&self) { Self::stop(self) }
}
