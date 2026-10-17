#![no_std]

//! Escrow contract with milestone-based fund release.
//! Client locks funds with `create_job`, then releases them per milestone.

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env, String,
    Symbol, Vec,
};

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct FundsReleased {
    pub project_id: String,
    pub report_hash: BytesN<32>,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum JobStatus {
    Escrowed,
    PartiallyReleased,
    Completed,
    Disputed,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Milestone {
    pub name: String,
    pub percentage: u32, // 0-100
    pub released: bool,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub client: Address,
    pub freelancer: Address,
    pub token: Address,
    pub amount: i128,
    pub status: JobStatus,
    pub milestones: Vec<Milestone>,
    pub disputed: bool,
    pub release_after: u32,
}

#[contracttype]
pub enum DataKey {
    Job(String),
    Admin,
    ProposedAdmin,
    GreenPayContractId,
}

pub const RELEASE_AFTER_LEDGERS: u32 = 10;

fn percentage_amount(amount: i128, percentage: u32) -> i128 {
    let percentage = percentage as i128;
    let whole = (amount / 100)
        .checked_mul(percentage)
        .expect("Milestone amount overflow");
    let fractional = (amount % 100)
        .checked_mul(percentage)
        .expect("Milestone amount overflow")
        / 100;
    whole
        .checked_add(fractional)
        .expect("Milestone amount overflow")
}

fn milestone_amount(amount: i128, milestones: &Vec<Milestone>, milestone_index: u32) -> i128 {
    let mut cumulative_percentage = 0u32;
    let mut previous_amount = 0i128;
    let mut index = 0u32;

    while index <= milestone_index {
        let milestone = milestones.get(index).expect("Invalid milestone index");
        cumulative_percentage = cumulative_percentage
            .checked_add(milestone.percentage)
            .expect("Milestone percentage overflow");
        if cumulative_percentage > 100 {
            panic!("Milestones must sum to 100%");
        }

        let cumulative_amount = percentage_amount(amount, cumulative_percentage);
        if index == milestone_index {
            return cumulative_amount
                .checked_sub(previous_amount)
                .expect("Milestone amount underflow");
        }

        previous_amount = cumulative_amount;
        index += 1;
    }

    panic!("Invalid milestone index");
}

#[contract]
pub struct EscrowContract;

#[contractimpl]
impl EscrowContract {
    /// Initialize contract with admin address.
    pub fn initialize(env: Env, admin: Address) {
        admin.require_auth();
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("Already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Set the GreenPay contract ID for pause checking.
    /// Only the admin can set this.
    pub fn set_greenpay_contract(env: Env, admin: Address, greenpay_contract_id: Address) {
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        if stored_admin != admin {
            panic!("Only admin can set GreenPay contract");
        }
        env.storage()
            .instance()
            .set(&DataKey::GreenPayContractId, &greenpay_contract_id);
    }

    /// Get the configured GreenPay contract ID.
    pub fn get_greenpay_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::GreenPayContractId)
    }

    /// Internal helper to check if the main GreenPay contract is paused.
    /// If no GreenPay contract is configured, returns false (allows operations).
    fn check_pause_state(env: &Env) {
        if let Some(greenpay_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::GreenPayContractId)
        {
            // Call is_paused() on the GreenPay contract using invoke_contract
            let is_paused: bool = env.invoke_contract(
                &greenpay_addr,
                &Symbol::new(env, "is_paused"),
                soroban_sdk::vec![env],
            );
            if is_paused {
                panic!("GreenPay contract is paused");
            }
        }
    }

    /// Propose a new admin address. Only the current admin can propose.
    pub fn propose_admin(env: Env, current_admin: Address, new_admin: Address) {
        current_admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        if stored_admin != current_admin {
            panic!("Only admin can propose new admin");
        }
        env.storage()
            .instance()
            .set(&DataKey::ProposedAdmin, &new_admin);
    }

    /// Accept the proposed admin role. Only the proposed new admin can accept.
    pub fn accept_admin(env: Env, new_admin: Address) {
        new_admin.require_auth();
        let proposed_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::ProposedAdmin)
            .expect("No proposed admin");
        if proposed_admin != new_admin {
            panic!("Not the proposed admin");
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::ProposedAdmin);
    }

    /// Get the current admin address.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized")
    }

    /// Get the proposed admin address, if any.
    pub fn get_proposed_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::ProposedAdmin)
    }

    /// Client funds escrow with milestones: transfers `amount` of `token` from client into this contract.
    pub fn create_job(
        env: Env,
        client: Address,
        freelancer: Address,
        job_id: String,
        token: Address,
        amount: i128,
        milestones: Vec<Milestone>,
    ) {
        client.require_auth();
        Self::check_pause_state(&env);

        if amount <= 0 {
            panic!("Amount must be positive");
        }
        if env.storage().instance().has(&DataKey::Job(job_id.clone())) {
            panic!("Job already exists");
        }

        // Validate milestones sum to 100%
        let mut total_percentage: u32 = 0;
        for milestone in milestones.iter() {
            if milestone.released {
                panic!("New milestones cannot already be released");
            }
            total_percentage = total_percentage
                .checked_add(milestone.percentage)
                .expect("Milestone percentage overflow");
        }
        if total_percentage != 100 {
            panic!("Milestones must sum to 100%");
        }

        let release_after = env
            .ledger()
            .sequence()
            .checked_add(RELEASE_AFTER_LEDGERS)
            .expect("Release ledger overflow");
        let token_client = token::Client::new(&env, &token);
        let contract_addr = env.current_contract_address();
        token_client.transfer(&client, &contract_addr, &amount);

        let job = Job {
            id: job_id.clone(),
            client: client.clone(),
            freelancer,
            token: token.clone(),
            amount,
            status: JobStatus::Escrowed,
            milestones,
            disputed: false,
            release_after,
        };
        env.storage().instance().set(&DataKey::Job(job_id), &job);
    }

    /// Client releases a specific milestone. Pays proportional XLM to freelancer.
    pub fn release_milestone(env: Env, client: Address, job_id: String, milestone_index: u32) {
        client.require_auth();
        Self::check_pause_state(&env);

        let mut job: Job = env
            .storage()
            .instance()
            .get(&DataKey::Job(job_id.clone()))
            .expect("Job not found");

        if job.client != client {
            panic!("Only the client can release");
        }
        if job.disputed {
            panic!("Job is disputed; admin must resolve");
        }
        if milestone_index >= job.milestones.len() {
            panic!("Invalid milestone index");
        }

        let milestone = job.milestones.get(milestone_index).unwrap();
        if milestone.released {
            panic!("Milestone already released");
        }
        if job.status == JobStatus::Completed {
            panic!("Job is already completed");
        }

        // Calculate proportional amount
        let release_amount = milestone_amount(job.amount, &job.milestones, milestone_index);

        // The token transfer and job update are one Soroban invocation: on transfer failure
        // the ledger rolls back both, leaving this milestone available for a safe retry.
        if release_amount > 0 {
            let token_client = token::Client::new(&env, &job.token);
            let contract_addr = env.current_contract_address();
            token_client.transfer(&contract_addr, &job.freelancer, &release_amount);
        }

        // Mark milestone as released and count total released
        let mut updated_milestones: Vec<Milestone> = Vec::new(&env);
        let mut released_count = 0u32;
        let len = job.milestones.len();
        let mut idx = 0u32;
        while idx < len {
            let mut m = job.milestones.get(idx).unwrap();
            if idx == milestone_index {
                m.released = true;
            }
            if m.released {
                released_count = released_count
                    .checked_add(1)
                    .expect("released_count overflow");
            }
            updated_milestones.push_back(m);
            idx += 1;
        }
        job.milestones = updated_milestones;

        // Update job status
        if released_count == len {
            job.status = JobStatus::Completed;
        } else {
            job.status = JobStatus::PartiallyReleased;
        }

        env.storage().instance().set(&DataKey::Job(job_id), &job);
    }

    /// Client or freelancer: Mark a job as disputed, freezing remaining releases.
    pub fn raise_dispute(env: Env, client: Address, job_id: String) {
        client.require_auth();
        Self::check_pause_state(&env);

        let mut job: Job = env
            .storage()
            .instance()
            .get(&DataKey::Job(job_id.clone()))
            .expect("Job not found");

        if job.client != client && job.freelancer != client {
            panic!("Only client or freelancer can raise dispute");
        }
        if job.status == JobStatus::Completed {
            panic!("Job is already completed");
        }
        job.disputed = true;
        job.status = JobStatus::Disputed;
        env.storage().instance().set(&DataKey::Job(job_id), &job);
    }

    /// Admin-only: Resolve a dispute and release remaining funds.
    pub fn resolve_dispute(env: Env, admin: Address, job_id: String, release_to_freelancer: bool) {
        admin.require_auth();
        Self::check_pause_state(&env);

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        if stored_admin != admin {
            panic!("Only admin can resolve disputes");
        }

        let mut job: Job = env
            .storage()
            .instance()
            .get(&DataKey::Job(job_id.clone()))
            .expect("Job not found");

        if !job.disputed {
            panic!("Job is not disputed");
        }

        let mut remaining_amount = 0i128;
        let mut updated_milestones = Vec::new(&env);
        let mut index = 0u32;
        for mut milestone in job.milestones.iter() {
            if !milestone.released {
                remaining_amount = remaining_amount
                    .checked_add(milestone_amount(job.amount, &job.milestones, index))
                    .expect("Remaining amount overflow");
                milestone.released = true;
            }
            updated_milestones.push_back(milestone);
            index += 1;
        }

        if remaining_amount > 0 {
            let recipient = if release_to_freelancer {
                &job.freelancer
            } else {
                &job.client
            };
            let token_client = token::Client::new(&env, &job.token);
            let contract_addr = env.current_contract_address();
            token_client.transfer(&contract_addr, recipient, &remaining_amount);
        }

        job.milestones = updated_milestones;
        job.status = JobStatus::Completed;
        job.disputed = false;
        env.storage().instance().set(&DataKey::Job(job_id), &job);
    }

    /// Freelancer can claim a milestone after release_after ledgers if not disputed.
    pub fn claim_milestone(env: Env, freelancer: Address, job_id: String, milestone_index: u32) {
        freelancer.require_auth();
        Self::check_pause_state(&env);

        let mut job: Job = env
            .storage()
            .instance()
            .get(&DataKey::Job(job_id.clone()))
            .expect("Job not found");

        if job.freelancer != freelancer {
            panic!("Only the freelancer can claim");
        }
        if job.disputed {
            panic!("Job is disputed; cannot claim milestone");
        }
        if env.ledger().sequence() < job.release_after {
            panic!("Release period not reached");
        }
        if milestone_index >= job.milestones.len() {
            panic!("Invalid milestone index");
        }
        let milestone = job.milestones.get(milestone_index).unwrap();
        if milestone.released {
            panic!("Milestone already released");
        }
        if job.status == JobStatus::Completed {
            panic!("Job is already completed");
        }
        // Calculate amount
        let release_amount = milestone_amount(job.amount, &job.milestones, milestone_index);
        if release_amount > 0 {
            let token_client = token::Client::new(&env, &job.token);
            let contract_addr = env.current_contract_address();
            token_client.transfer(&contract_addr, &job.freelancer, &release_amount);
        }

        // Mark as released
        let mut updated_milestones: Vec<Milestone> = Vec::new(&env);
        let len = job.milestones.len();
        let mut all_released = true;
        let mut i = 0u32;
        while i < len {
            let mut m = job.milestones.get(i).unwrap();
            if i == milestone_index {
                m.released = true;
            }
            if !m.released {
                all_released = false;
            }
            updated_milestones.push_back(m);
            i += 1;
        }
        job.milestones = updated_milestones;

        // Update status
        job.status = if all_released {
            JobStatus::Completed
        } else {
            JobStatus::PartiallyReleased
        };

        env.storage().instance().set(&DataKey::Job(job_id), &job);
    }

    /// Release funds for a job upon verification of completion evidence, anchoring the SHA-256 report hash.
    pub fn release_funds(
        env: Env,
        caller: Address,
        job_id: String,
        project_report_hash: BytesN<32>,
    ) -> i128 {
        caller.require_auth();
        Self::check_pause_state(&env);

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");

        let mut job: Job = env
            .storage()
            .instance()
            .get(&DataKey::Job(job_id.clone()))
            .expect("Job not found");

        if caller != stored_admin && caller != job.client {
            panic!("Only admin or client can release funds");
        }

        if job.disputed {
            panic!("Job is disputed; cannot release funds");
        }

        let mut total_unreleased: i128 = 0;
        let mut has_unreleased = false;
        let mut updated_milestones: Vec<Milestone> = Vec::new(&env);
        let mut index = 0u32;
        for milestone in job.milestones.iter() {
            let mut m = milestone.clone();
            if !m.released {
                has_unreleased = true;
                let amount = milestone_amount(job.amount, &job.milestones, index);
                total_unreleased = total_unreleased
                    .checked_add(amount)
                    .expect("Remaining amount overflow");
                m.released = true;
            }
            updated_milestones.push_back(m);
            index += 1;
        }

        if !has_unreleased {
            panic!("No unreleased funds available");
        }

        if total_unreleased > 0 {
            let token_client = token::Client::new(&env, &job.token);
            let contract_addr = env.current_contract_address();
            token_client.transfer(&contract_addr, &job.freelancer, &total_unreleased);
        }

        job.milestones = updated_milestones;
        job.status = JobStatus::Completed;
        env.storage()
            .instance()
            .set(&DataKey::Job(job_id.clone()), &job);

        // Emit FundsReleased event anchoring the project_report_hash
        env.events().publish(
            (symbol_short!("funds_rel"), job_id.clone()),
            FundsReleased {
                project_id: job_id,
                report_hash: project_report_hash,
                amount: total_unreleased,
            },
        );

        total_unreleased
    }

    pub fn get_job(env: Env, job_id: String) -> Option<Job> {
        env.storage().instance().get(&DataKey::Job(job_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, Env, String, Vec};

    fn setup(env: &Env) -> (Address, EscrowContractClient) {
        let cid = env.register_contract(None, EscrowContract);
        let client = EscrowContractClient::new(env, &cid);
        let admin = Address::generate(env);
        client.initialize(&admin);
        (admin, client)
    }

    #[test]
    fn test_initialize_requires_admin_auth() {
        let env = Env::default();
        let cid = env.register_contract(None, EscrowContract);
        let client = EscrowContractClient::new(&env, &cid);
        let admin = Address::generate(&env);

        assert!(client.try_initialize(&admin).is_err());
        assert!(client.try_get_admin().is_err());
    }

    #[test]
    fn test_admin_rotation_success() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);
        let new_admin = Address::generate(&env);

        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_proposed_admin(), None);

        client.propose_admin(&admin, &new_admin);
        assert_eq!(client.get_proposed_admin(), Some(new_admin.clone()));

        client.accept_admin(&new_admin);
        assert_eq!(client.get_admin(), new_admin);
        assert_eq!(client.get_proposed_admin(), None);
    }

    #[test]
    #[should_panic(expected = "Only admin can propose new admin")]
    fn test_propose_admin_unauthorized_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);
        let impostor = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.propose_admin(&impostor, &new_admin);
    }

    #[test]
    #[should_panic(expected = "Not the proposed admin")]
    fn test_accept_admin_wrong_address_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);
        let new_admin = Address::generate(&env);
        let wrong_admin = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&wrong_admin);
    }

    #[test]
    #[should_panic(expected = "No proposed admin")]
    fn test_accept_admin_no_proposal_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);
        let new_admin = Address::generate(&env);

        client.accept_admin(&new_admin);
    }

    #[test]
    fn test_milestone_based_release() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        // Use a real Stellar asset token so create_job's transfer succeeds
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-1");

        // Create 3 milestones: 50%, 30%, 20%
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Design"),
            percentage: 50,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Development"),
            percentage: 30,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Testing"),
            percentage: 20,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Escrowed);
        assert_eq!(job.milestones.len(), 3);
    }

    #[test]
    fn test_milestone_rounding_releases_exact_escrow_amount_out_of_order() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &101i128);
        let job_id = String::from_str(&env, "job-rounded");

        let mut milestones = Vec::new(&env);
        for percentage in [33, 33, 34].iter() {
            milestones.push_back(Milestone {
                name: String::from_str(&env, "Milestone"),
                percentage: *percentage,
                released: false,
            });
        }

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &101i128,
            &milestones,
        );
        client.release_milestone(&client_addr, &job_id, &2u32);
        client.release_milestone(&client_addr, &job_id, &0u32);
        client.release_milestone(&client_addr, &job_id, &1u32);

        let token_client = soroban_sdk::token::Client::new(&env, &token);
        assert_eq!(token_client.balance(&freelancer), 101i128);
        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Completed);
        for milestone in job.milestones.iter() {
            assert!(milestone.released);
        }
    }

    #[test]
    fn test_failed_token_transfer_preserves_milestone_for_retry() {
        let env = Env::default();
        env.mock_all_auths();
        let cid = env.register_contract(None, EscrowContract);
        let client = EscrowContractClient::new(&env, &cid);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        token_admin_client.mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-retry");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "All"),
            percentage: 100,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );
        token_admin_client.clawback(&cid, &1000i128);

        let failed = client.try_release_milestone(&client_addr, &job_id, &0u32);
        assert!(failed.is_err());
        let job = client
            .get_job(&job_id)
            .expect("Job should remain persisted");
        assert_eq!(job.status, JobStatus::Escrowed);
        assert!(!job.milestones.get(0).unwrap().released);
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            0
        );

        token_admin_client.mint(&cid, &1000i128);
        client.release_milestone(&client_addr, &job_id, &0u32);
        let job = client
            .get_job(&job_id)
            .expect("Job should remain persisted");
        assert_eq!(job.status, JobStatus::Completed);
        assert!(job.milestones.get(0).unwrap().released);
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            1000
        );
    }

    #[test]
    #[should_panic(expected = "Only the freelancer can claim")]
    fn test_claim_milestone_rejects_non_freelancer() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let unauthorized = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &100i128);
        let job_id = String::from_str(&env, "job-unauthorized-claim");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "All"),
            percentage: 100,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &100i128,
            &milestones,
        );
        client.claim_milestone(&unauthorized, &job_id, &0u32);
    }

    #[test]
    fn test_dispute_refund_marks_milestones_settled() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &100i128);
        let job_id = String::from_str(&env, "job-refund");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "First"),
            percentage: 40,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Second"),
            percentage: 60,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &100i128,
            &milestones,
        );
        client.release_milestone(&client_addr, &job_id, &0u32);
        client.raise_dispute(&client_addr, &job_id);
        client.resolve_dispute(&_admin, &job_id, &false);

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Completed);
        assert!(!job.disputed);
        for milestone in job.milestones.iter() {
            assert!(milestone.released);
        }
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&client_addr),
            60
        );
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            40
        );
    }

    #[test]
    fn test_failed_dispute_settlement_preserves_remaining_milestones_for_retry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, EscrowContract);
        let client = EscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        token_admin_client.mint(&client_addr, &100i128);
        let job_id = String::from_str(&env, "job-dispute-retry");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "First"),
            percentage: 40,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Second"),
            percentage: 60,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &100i128,
            &milestones,
        );
        client.release_milestone(&client_addr, &job_id, &0u32);
        client.raise_dispute(&client_addr, &job_id);
        token_admin_client.clawback(&contract_id, &60i128);

        assert!(client.try_resolve_dispute(&admin, &job_id, &true).is_err());
        let job = client
            .get_job(&job_id)
            .expect("Job should remain persisted");
        assert_eq!(job.status, JobStatus::Disputed);
        assert!(job.disputed);
        assert!(job.milestones.get(0).unwrap().released);
        assert!(!job.milestones.get(1).unwrap().released);
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            40
        );

        token_admin_client.mint(&contract_id, &60i128);
        client.resolve_dispute(&admin, &job_id, &true);
        let job = client
            .get_job(&job_id)
            .expect("Job should remain persisted");
        assert_eq!(job.status, JobStatus::Completed);
        assert!(!job.disputed);
        for milestone in job.milestones.iter() {
            assert!(milestone.released);
        }
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            100
        );
    }

    #[test]
    fn test_dispute_release_marks_milestones_settled_and_blocks_replay() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &100i128);
        let job_id = String::from_str(&env, "job-dispute-release");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "First"),
            percentage: 25,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Second"),
            percentage: 75,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &100i128,
            &milestones,
        );
        client.raise_dispute(&client_addr, &job_id);
        client.resolve_dispute(&admin, &job_id, &true);

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Completed);
        assert!(!job.disputed);
        for milestone in job.milestones.iter() {
            assert!(milestone.released);
        }
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            100
        );
        assert!(client
            .try_release_milestone(&client_addr, &job_id, &0u32)
            .is_err());
    }

    #[test]
    fn test_release_funds_settles_zero_value_rounding_remainder() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1i128);
        let job_id = String::from_str(&env, "job-zero-remainder");
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Rounded to zero"),
            percentage: 99,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Final unit"),
            percentage: 1,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1i128,
            &milestones,
        );
        client.release_milestone(&client_addr, &job_id, &1u32);
        let released =
            client.release_funds(&admin, &job_id, &BytesN::from_array(&env, &[0x42; 32]));

        assert_eq!(released, 0);
        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Completed);
        for milestone in job.milestones.iter() {
            assert!(milestone.released);
        }
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &token).balance(&freelancer),
            1
        );
    }

    #[test]
    fn test_percentage_amount_handles_maximum_i128_without_overflow() {
        assert_eq!(percentage_amount(i128::MAX, 100), i128::MAX);
        let one_percent_rounded_up = i128::MAX / 100 + if i128::MAX % 100 == 0 { 0 } else { 1 };
        assert_eq!(
            percentage_amount(i128::MAX, 99),
            i128::MAX - one_percent_rounded_up
        );

        let env = Env::default();
        let mut milestones = Vec::new(&env);
        for percentage in [33u32, 33, 34].iter() {
            milestones.push_back(Milestone {
                name: String::from_str(&env, "Boundary"),
                percentage: *percentage,
                released: false,
            });
        }
        let total = (0..3).fold(0i128, |sum, index| {
            sum + milestone_amount(i128::MAX, &milestones, index)
        });
        assert_eq!(total, i128::MAX);
    }

    #[test]
    #[should_panic(expected = "Milestones must sum to 100%")]
    fn test_milestone_validation() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token = Address::generate(&env);
        let job_id = String::from_str(&env, "job-invalid");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M1"),
            percentage: 50,
            released: false,
        });
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M2"),
            percentage: 40,
            released: false,
        });
        // Only 90%, should panic

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );
    }

    #[test]
    #[should_panic(expected = "New milestones cannot already be released")]
    fn test_create_job_rejects_pre_released_milestone() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);
        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let job_id = String::from_str(&env, "job-pre-released");
        let token = Address::generate(&env);
        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Already paid"),
            percentage: 100,
            released: true,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );
    }

    #[test]
    #[should_panic(expected = "Job not found")]
    fn release_missing_job_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);
        let addr = Address::generate(&env);
        client.release_milestone(&addr, &String::from_str(&env, "no-such-job"), &0u32);
    }

    #[test]
    fn test_dispute_freezes_release() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        // Use a real Stellar asset token so create_job's transfer succeeds
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-dispute");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M1"),
            percentage: 100,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        // Dispute the job
        client.raise_dispute(&client_addr, &job_id);

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Disputed);
        assert!(job.disputed);
    }

    #[test]
    fn test_release_funds_with_evidence_hash_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-evidence-1");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "Deliverable"),
            percentage: 100,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        let report_hash = BytesN::from_array(&env, &[0xab; 32]);
        let released = client.release_funds(&admin, &job_id, &report_hash);
        assert_eq!(released, 1000i128);

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Completed);
        assert!(job.milestones.get(0).unwrap().released);

        let balance = soroban_sdk::token::Client::new(&env, &token).balance(&freelancer);
        assert_eq!(balance, 1000i128);
    }

    #[test]
    #[should_panic(expected = "Only admin or client can release funds")]
    fn test_release_funds_unauthorized_caller_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let unauthorized = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-unauth");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "All"),
            percentage: 100,
            released: false,
        });

        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        let report_hash = BytesN::from_array(&env, &[0xcd; 32]);
        client.release_funds(&unauthorized, &job_id, &report_hash);
    }

    #[test]
    fn test_set_greenpay_contract() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);
        let greenpay_addr = Address::generate(&env);

        assert_eq!(client.get_greenpay_contract(), None);

        client.set_greenpay_contract(&admin, &greenpay_addr);
        assert_eq!(client.get_greenpay_contract(), Some(greenpay_addr.clone()));
    }

    #[test]
    #[should_panic(expected = "Only admin can set GreenPay contract")]
    fn test_set_greenpay_contract_unauthorized_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, client) = setup(&env);
        let unauthorized = Address::generate(&env);
        let greenpay_addr = Address::generate(&env);

        client.set_greenpay_contract(&unauthorized, &greenpay_addr);
    }

    #[test]
    #[should_panic(expected = "GreenPay contract is paused")]
    fn test_create_job_fails_when_greenpay_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        // Register a mock GreenPay contract that returns is_paused = true
        let greenpay_cid = env.register_contract(None, MockPausedGreenPayContract);

        // Initialize the mock contract to return paused = true
        env.invoke_contract::<()>(
            &greenpay_cid,
            &Symbol::new(&env, "initialize"),
            soroban_sdk::vec![&env, true.into()],
        );

        client.set_greenpay_contract(&admin, &greenpay_cid);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-paused");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M1"),
            percentage: 100,
            released: false,
        });

        // This should panic with "GreenPay contract is paused"
        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );
    }

    #[test]
    #[should_panic(expected = "GreenPay contract is paused")]
    fn test_release_milestone_fails_when_greenpay_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-pause-release");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M1"),
            percentage: 100,
            released: false,
        });

        // Create job first (without pause)
        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        // Now set up paused GreenPay contract
        let greenpay_cid = env.register_contract(None, MockPausedGreenPayContract);

        // Initialize the mock contract to return paused = true
        env.invoke_contract::<()>(
            &greenpay_cid,
            &Symbol::new(&env, "initialize"),
            soroban_sdk::vec![&env, true.into()],
        );

        client.set_greenpay_contract(&admin, &greenpay_cid);

        // This should panic with "GreenPay contract is paused"
        client.release_milestone(&client_addr, &job_id, &0u32);
    }

    #[test]
    fn test_operations_succeed_when_greenpay_not_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);

        // Register a mock GreenPay contract that returns is_paused = false
        let greenpay_cid = env.register_contract(None, MockPausedGreenPayContract);

        // Initialize the mock contract to return paused = false
        env.invoke_contract::<()>(
            &greenpay_cid,
            &Symbol::new(&env, "initialize"),
            soroban_sdk::vec![&env, false.into()],
        );

        client.set_greenpay_contract(&admin, &greenpay_cid);

        let client_addr = Address::generate(&env);
        let freelancer = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&client_addr, &1000i128);
        let job_id = String::from_str(&env, "job-not-paused");

        let mut milestones = Vec::new(&env);
        milestones.push_back(Milestone {
            name: String::from_str(&env, "M1"),
            percentage: 100,
            released: false,
        });

        // This should succeed
        client.create_job(
            &client_addr,
            &freelancer,
            &job_id,
            &token,
            &1000i128,
            &milestones,
        );

        let job = client.get_job(&job_id).expect("Job should exist");
        assert_eq!(job.status, JobStatus::Escrowed);
    }

    // ─── Mock GreenPay Contract for Testing Pause ────────────────────────────────

    // Test-only. `#[contractimpl]` emits one exported WASM symbol per function, so a
    // non-test build would define `initialize` here as well as on EscrowContract
    // and the release wasm32 build fails with "symbol `initialize` is already
    // defined". Host `cargo check`/`cargo test` do not export symbols, which is why
    // only the wasm build catches it.
    #[cfg(test)]
    #[contract]
    pub struct MockPausedGreenPayContract;

    #[cfg(test)]
    #[contractimpl]
    impl MockPausedGreenPayContract {
        pub fn initialize(env: Env, paused: bool) {
            env.storage()
                .instance()
                .set(&symbol_short!("paused"), &paused);
        }

        pub fn is_paused(env: Env) -> bool {
            env.storage()
                .instance()
                .get(&symbol_short!("paused"))
                .unwrap_or(false)
        }
    }
}
