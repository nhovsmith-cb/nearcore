use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use near_crypto::PublicKey;
use near_primitives::account::{AccessKey, Account};
use near_primitives::epoch_manager::{EpochConfig, EpochConfigStore};
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::ShardLayout;
use near_primitives::state_record::StateRecord;
use near_primitives::test_utils::{create_test_signer, create_user_test_signer};
use near_primitives::types::{
    AccountId, AccountInfo, Balance, BlockHeight, BlockHeightDelta, Gas, NumBlocks, NumSeats,
    ProtocolVersion,
};
use near_primitives::utils::from_timestamp;
use near_primitives::version::PROTOCOL_VERSION;
use near_time::Clock;
use num_rational::Rational32;

use crate::{Genesis, GenesisConfig, GenesisContents, GenesisRecords};

/// A builder for constructing a valid genesis for testing.
///
/// The philosophy is that this can be used to generate a genesis that is
/// consistent, with flexibility to override specific settings, and with
/// defaults that are likely to be reasonable.
///
/// For parameters that are especially difficult to set correctly, the builder
/// should provide the ability to set them in a more intuitive way. For example,
/// since the validator selection algorithm is rather tricky, the builder
/// provides an option to specify exactly which accounts should be block and
/// chunk-only producers.
#[derive(Default, Clone, Debug)]
pub struct TestGenesisBuilder {
    chain_id: Option<String>,
    genesis_time: Option<chrono::DateTime<chrono::Utc>>,
    protocol_version: Option<ProtocolVersion>,
    genesis_height: Option<BlockHeight>,
    epoch_length: Option<BlockHeightDelta>,
    min_max_gas_price: Option<(Balance, Balance)>,
    gas_limit: Option<Gas>,
    transaction_validity_period: Option<NumBlocks>,
    validators: Option<ValidatorsSpec>,
    protocol_treasury_account: Option<String>,
    max_inflation_rate: Option<Rational32>,
    user_accounts: Vec<UserAccount>,
    epoch_config: Option<EpochConfig>,
}

#[derive(Debug, Clone)]
enum ValidatorsSpec {
    DesiredRoles {
        block_and_chunk_producers: Vec<String>,
        chunk_validators_only: Vec<String>,
    },
    Raw {
        validators: Vec<AccountInfo>,
        num_block_producer_seats: NumSeats,
        num_chunk_producer_seats: NumSeats,
        num_chunk_validator_seats: NumSeats,
    },
}

#[derive(Debug, Clone)]
struct UserAccount {
    account_id: AccountId,
    balance: Balance,
    access_keys: Vec<PublicKey>,
}

impl TestGenesisBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn epoch_config_mut(&mut self) -> &mut EpochConfig {
        if self.epoch_config.is_none() {
            let mut epoch_config = Genesis::test_epoch_config(1, ShardLayout::single_shard(), 100);
            epoch_config.block_producer_kickout_threshold = 0;
            epoch_config.chunk_producer_kickout_threshold = 0;
            epoch_config.chunk_validator_only_kickout_threshold = 0;
            self.epoch_config = Some(epoch_config);
        }
        self.epoch_config.as_mut().unwrap()
    }

    pub fn chain_id(&mut self, chain_id: String) -> &mut Self {
        self.chain_id = Some(chain_id);
        self
    }

    pub fn genesis_time(&mut self, genesis_time: chrono::DateTime<chrono::Utc>) -> &mut Self {
        self.genesis_time = Some(genesis_time);
        self
    }

    pub fn genesis_time_from_clock(&mut self, clock: &Clock) -> &mut Self {
        self.genesis_time = Some(from_timestamp(clock.now_utc().unix_timestamp_nanos() as u64));
        self
    }

    pub fn protocol_version(&mut self, protocol_version: ProtocolVersion) -> &mut Self {
        self.protocol_version = Some(protocol_version);
        self
    }

    pub fn protocol_version_latest(&mut self) -> &mut Self {
        self.protocol_version = Some(PROTOCOL_VERSION);
        self
    }

    pub fn genesis_height(&mut self, genesis_height: BlockHeight) -> &mut Self {
        self.genesis_height = Some(genesis_height);
        self
    }

    pub fn epoch_length(&mut self, epoch_length: BlockHeightDelta) -> &mut Self {
        self.epoch_length = Some(epoch_length);
        self
    }

    pub fn shard_layout_single(&mut self) -> &mut Self {
        self.epoch_config_mut().shard_layout = ShardLayout::single_shard();
        self
    }

    pub fn shard_layout_simple_v1(&mut self, boundary_accounts: &[&str]) -> &mut Self {
        let boundary_accounts = boundary_accounts.iter().map(|a| a.parse().unwrap()).collect();
        self.epoch_config_mut().shard_layout =
            ShardLayout::multi_shard_custom(boundary_accounts, 1);
        self
    }

    // TODO(#11265): move this and relevant methods to epoch config builder.
    // In dynamic resharding world, shard layout will not be static.
    pub fn shard_layout(&mut self, shard_layout: ShardLayout) -> &mut Self {
        self.epoch_config_mut().shard_layout = shard_layout;
        self
    }

    pub fn gas_prices(&mut self, min: Balance, max: Balance) -> &mut Self {
        self.min_max_gas_price = Some((min, max));
        self
    }

    pub fn gas_prices_free(&mut self) -> &mut Self {
        self.min_max_gas_price = Some((0, 0));
        self
    }

    pub fn gas_limit(&mut self, gas_limit: Gas) -> &mut Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    pub fn gas_limit_one_petagas(&mut self) -> &mut Self {
        self.gas_limit = Some(1_000_000_000_000_000);
        self
    }

    pub fn transaction_validity_period(
        &mut self,
        transaction_validity_period: NumBlocks,
    ) -> &mut Self {
        self.transaction_validity_period = Some(transaction_validity_period);
        self
    }

    /// Specifies that we want the validators to be exactly the specified accounts.
    /// This will generate a reasonable set of parameters so that the given
    /// validators are selected as specified.
    pub fn validators_desired_roles(
        &mut self,
        block_and_chunk_producers: &[&str],
        chunk_validators_only: &[&str],
    ) -> &mut Self {
        self.validators = Some(ValidatorsSpec::DesiredRoles {
            block_and_chunk_producers: block_and_chunk_producers
                .iter()
                .map(|s| s.to_string())
                .collect(),
            chunk_validators_only: chunk_validators_only.iter().map(|s| s.to_string()).collect(),
        });
        self
    }

    /// Specifies the validator fields directly, relying on the validator selection
    /// algorithm to determine which validators are selected as block or chunk
    /// producers.
    pub fn validators_raw(
        &mut self,
        validators: Vec<AccountInfo>,
        num_block_producer_seats: NumSeats,
        num_chunk_producer_seats: NumSeats,
        num_chunk_validator_only_seats: NumSeats,
    ) -> &mut Self {
        let num_chunk_validator_seats =
            std::cmp::max(num_block_producer_seats, num_chunk_producer_seats)
                + num_chunk_validator_only_seats;
        self.validators = Some(ValidatorsSpec::Raw {
            validators,
            num_block_producer_seats,
            num_chunk_producer_seats,
            num_chunk_validator_seats,
        });
        self
    }

    pub fn minimum_stake_ratio(&mut self, minimum_stake_ratio: Rational32) -> &mut Self {
        self.epoch_config_mut().validator_selection_config.minimum_stake_ratio =
            minimum_stake_ratio;
        self
    }

    pub fn max_inflation_rate(&mut self, max_inflation_rate: Rational32) -> &mut Self {
        self.max_inflation_rate = Some(max_inflation_rate);
        self
    }

    pub fn minimum_validators_per_shard(
        &mut self,
        minimum_validators_per_shard: NumSeats,
    ) -> &mut Self {
        self.epoch_config_mut().validator_selection_config.minimum_validators_per_shard =
            minimum_validators_per_shard;
        self
    }

    pub fn target_validator_mandates_per_shard(
        &mut self,
        target_validator_mandates_per_shard: NumSeats,
    ) -> &mut Self {
        self.epoch_config_mut().target_validator_mandates_per_shard =
            target_validator_mandates_per_shard;
        self
    }

    /// Specifies the protocol treasury account. If not specified, this will
    /// pick an arbitrary account name and ensure that it is included in the
    /// genesis records.
    pub fn protocol_treasury_account(&mut self, protocol_treasury_account: String) -> &mut Self {
        self.protocol_treasury_account = Some(protocol_treasury_account);
        self
    }

    pub fn shuffle_shard_assignment_for_chunk_producers(&mut self, shuffle: bool) -> &mut Self {
        self.epoch_config_mut()
            .validator_selection_config
            .shuffle_shard_assignment_for_chunk_producers = shuffle;
        self
    }

    pub fn kickouts_disabled(&mut self) -> &mut Self {
        let epoch_config = self.epoch_config_mut();
        epoch_config.block_producer_kickout_threshold = 0;
        epoch_config.chunk_producer_kickout_threshold = 0;
        epoch_config.chunk_validator_only_kickout_threshold = 0;
        self
    }

    /// Validators with performance below 80% are kicked out, similarly to
    /// mainnet as of 28 Jun 2024.
    pub fn kickouts_standard_80_percent(&mut self) -> &mut Self {
        let epoch_config = self.epoch_config_mut();
        epoch_config.block_producer_kickout_threshold = 80;
        epoch_config.chunk_producer_kickout_threshold = 80;
        epoch_config.chunk_validator_only_kickout_threshold = 80;
        self
    }

    /// Only chunk validator-only nodes can be kicked out.
    pub fn kickouts_for_chunk_validators_only(&mut self) -> &mut Self {
        let epoch_config = self.epoch_config_mut();
        epoch_config.block_producer_kickout_threshold = 0;
        epoch_config.chunk_producer_kickout_threshold = 0;
        epoch_config.chunk_validator_only_kickout_threshold = 50;
        self
    }

    pub fn add_user_account_simple(
        &mut self,
        account_id: AccountId,
        balance: Balance,
    ) -> &mut Self {
        self.user_accounts.push(UserAccount {
            balance,
            access_keys: vec![create_user_test_signer(&account_id).public_key()],
            account_id,
        });
        self
    }

    pub fn build(mut self) -> (Genesis, EpochConfigStore) {
        let chain_id = self.chain_id.clone().unwrap_or_else(|| {
            let default = "test".to_string();
            tracing::warn!("Genesis chain_id not explicitly set, defaulting to {:?}.", default);
            default
        });
        let protocol_version = self.protocol_version.unwrap_or_else(|| {
            let default = PROTOCOL_VERSION;
            tracing::warn!("Genesis protocol_version not explicitly set, defaulting to latest protocol version {:?}.", default);
            default
        });
        let validator_specs = self.validators.clone().unwrap_or_else(|| {
            let default = ValidatorsSpec::DesiredRoles {
                block_and_chunk_producers: vec!["validator0".to_string()],
                chunk_validators_only: vec![],
            };
            tracing::warn!(
                "Genesis validators not explicitly set, defaulting to a single validator setup {:?}.",
                default
            );
            default
        });
        let epoch_length = self.epoch_length.unwrap_or_else(|| {
            let default = 100;
            tracing::warn!("Genesis epoch_length not explicitly set, defaulting to {:?}.", default);
            default
        });

        let derived_validator_setup = derive_validator_setup(validator_specs);

        let mut epoch_config = self.epoch_config_mut().clone();
        epoch_config.num_block_producer_seats = derived_validator_setup.num_block_producer_seats;
        epoch_config.validator_selection_config.num_chunk_producer_seats =
            derived_validator_setup.num_chunk_producer_seats;
        epoch_config.validator_selection_config.num_chunk_validator_seats =
            derived_validator_setup.num_chunk_validator_seats;
        let epoch_config_store = EpochConfigStore::test(BTreeMap::from_iter(vec![(
            protocol_version,
            Arc::new(epoch_config),
        )]));
        let shard_layout =
            epoch_config_store.get_config(protocol_version).as_ref().shard_layout.clone();

        let genesis_time = self.genesis_time.unwrap_or_else(|| {
            let default = chrono::Utc::now();
            tracing::warn!(
                "Genesis genesis_time not explicitly set, defaulting to current time {:?}.",
                default
            );
            default
        });

        let genesis_height = self.genesis_height.unwrap_or_else(|| {
            let default = 1;
            tracing::warn!(
                "Genesis genesis_height not explicitly set, defaulting to {:?}.",
                default
            );
            default
        });
        let (min_gas_price, max_gas_price) = self.min_max_gas_price.unwrap_or_else(|| {
            let default = (0, 0);
            tracing::warn!("Genesis gas prices not explicitly set, defaulting to free gas.");
            default
        });
        let gas_limit = self.gas_limit.unwrap_or_else(|| {
            let default = 1_000_000_000_000_000;
            tracing::warn!("Genesis gas_limit not explicitly set, defaulting to {:?}.", default);
            default
        });
        let transaction_validity_period = self.transaction_validity_period.unwrap_or_else(|| {
            let default = 100;
            tracing::warn!(
                "Genesis transaction_validity_period not explicitly set, defaulting to {:?}.",
                default
            );
            default
        });

        let protocol_treasury_account: AccountId = self
            .protocol_treasury_account
            .clone()
            .unwrap_or_else(|| {
                let default = "near".to_string();
                tracing::warn!(
                    "Genesis protocol_treasury_account not explicitly set, defaulting to {:?}.",
                    default
                );
                default
            })
            .parse()
            .unwrap();
        let max_inflation_rate = self.max_inflation_rate.unwrap_or_else(|| {
            let default = Rational32::new(1, 1);
            tracing::warn!(
                "Genesis max_inflation_rate not explicitly set, defaulting to {:?}.",
                default
            );
            default
        });

        if self
            .user_accounts
            .iter()
            .map(|account| &account.account_id)
            .collect::<HashSet<_>>()
            .len()
            != self.user_accounts.len()
        {
            panic!("Duplicate user accounts specified.");
        }

        // We will merge the user accounts that were specified, with the
        // validator staking accounts from the validator setup, and ensure
        // that the protocol treasury account is included too. We will use all
        // of this to generate the genesis records and also calculate the
        // total supply.
        let mut user_accounts = self.user_accounts.clone();
        if user_accounts.iter().all(|account| &account.account_id != &protocol_treasury_account) {
            tracing::warn!(
                "Protocol treasury account {:?} not found in user accounts;
                to keep genesis valid, adding it as a user account with zero balance.",
                protocol_treasury_account
            );
            user_accounts.push(UserAccount {
                account_id: protocol_treasury_account.clone(),
                balance: 0,
                access_keys: vec![],
            });
        }

        let mut total_supply = 0;
        let mut validator_stake: HashMap<AccountId, Balance> = HashMap::new();
        for validator in &derived_validator_setup.validators {
            total_supply += validator.amount;
            validator_stake.insert(validator.account_id.clone(), validator.amount);
        }
        let mut records = Vec::new();
        for user_account in &user_accounts {
            total_supply += user_account.balance;
            records.push(StateRecord::Account {
                account_id: user_account.account_id.clone(),
                account: Account::new(
                    user_account.balance,
                    validator_stake.remove(&user_account.account_id).unwrap_or(0),
                    0,
                    CryptoHash::default(),
                    0,
                    protocol_version,
                ),
            });
            for access_key in &user_account.access_keys {
                records.push(StateRecord::AccessKey {
                    account_id: user_account.account_id.clone(),
                    public_key: access_key.clone(),
                    access_key: AccessKey {
                        nonce: 0,
                        permission: near_primitives::account::AccessKeyPermission::FullAccess,
                    },
                });
            }
        }
        for (account_id, balance) in validator_stake {
            records.push(StateRecord::Account {
                account_id,
                account: Account::new(0, balance, 0, CryptoHash::default(), 0, protocol_version),
            });
        }

        // NOTE: If you want to override any of the hardcoded defaults below,
        // follow the same pattern and add a corresponding `Option` field to the builder,
        // and add the corresponding functions to set the field. DO NOT just modify
        // the defaults.
        let genesis_config = GenesisConfig {
            chain_id,
            genesis_time,
            genesis_height,
            epoch_length,
            min_gas_price,
            max_gas_price,
            gas_limit,
            dynamic_resharding: false,
            fishermen_threshold: 0,
            transaction_validity_period,
            protocol_version,
            protocol_treasury_account,
            online_min_threshold: Rational32::new(90, 100),
            online_max_threshold: Rational32::new(99, 100),
            gas_price_adjustment_rate: Rational32::new(0, 1),
            num_blocks_per_year: 86400,
            protocol_reward_rate: Rational32::new(0, 1),
            total_supply,
            max_kickout_stake_perc: 100,
            validators: derived_validator_setup.validators,
            shard_layout: shard_layout.clone(),
            num_block_producer_seats: derived_validator_setup.num_block_producer_seats,
            num_block_producer_seats_per_shard: shard_layout
                .shard_ids()
                .map(|_| derived_validator_setup.num_block_producer_seats)
                .collect(),
            num_chunk_only_producer_seats: 0,
            minimum_stake_divisor: 10,
            max_inflation_rate,
            protocol_upgrade_stake_threshold: Rational32::new(8, 10),
            num_chunk_producer_seats: derived_validator_setup.num_chunk_producer_seats,
            num_chunk_validator_seats: derived_validator_setup.num_chunk_validator_seats,
            chunk_producer_assignment_changes_limit: 5,
            ..Default::default()
        };

        (
            Genesis {
                config: genesis_config,
                contents: GenesisContents::Records { records: GenesisRecords(records) },
            },
            epoch_config_store,
        )
    }
}

struct DerivedValidatorSetup {
    validators: Vec<AccountInfo>,
    num_block_producer_seats: NumSeats,
    num_chunk_producer_seats: NumSeats,
    num_chunk_validator_seats: NumSeats,
}

const ONE_NEAR: Balance = 1_000_000_000_000_000_000_000_000;

fn derive_validator_setup(specs: ValidatorsSpec) -> DerivedValidatorSetup {
    match specs {
        ValidatorsSpec::DesiredRoles { block_and_chunk_producers, chunk_validators_only } => {
            let num_block_and_chunk_producer_seats = block_and_chunk_producers.len() as NumSeats;
            let num_chunk_validator_only_seats = chunk_validators_only.len() as NumSeats;
            let mut validators = Vec::new();
            for i in 0..num_block_and_chunk_producer_seats as usize {
                let account_id: AccountId = block_and_chunk_producers[i].parse().unwrap();
                let account_info = AccountInfo {
                    public_key: create_test_signer(account_id.as_str()).public_key(),
                    account_id,
                    amount: ONE_NEAR * (10000 - i as Balance),
                };
                validators.push(account_info);
            }
            for i in 0..num_chunk_validator_only_seats as usize {
                let account_id: AccountId = chunk_validators_only[i].parse().unwrap();
                let account_info = AccountInfo {
                    public_key: create_test_signer(account_id.as_str()).public_key(),
                    account_id,
                    amount: ONE_NEAR
                        * (10000 - i as Balance - num_block_and_chunk_producer_seats as Balance),
                };
                validators.push(account_info);
            }
            DerivedValidatorSetup {
                validators,
                num_block_producer_seats: num_block_and_chunk_producer_seats,
                num_chunk_producer_seats: num_block_and_chunk_producer_seats,
                num_chunk_validator_seats: num_block_and_chunk_producer_seats
                    + num_chunk_validator_only_seats,
            }
        }
        ValidatorsSpec::Raw {
            validators,
            num_block_producer_seats,
            num_chunk_producer_seats,
            num_chunk_validator_seats,
        } => DerivedValidatorSetup {
            validators,
            num_block_producer_seats,
            num_chunk_producer_seats,
            num_chunk_validator_seats,
        },
    }
}
