use super::super::positions;
use super::{accounts::VehntBalance, *};
use crate::cli::positions::PositionOwners;
use crate::types::SubDao;
use axum::{
    body::{self, Empty, Full},
    extract::Path,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
};
use solana_sdk::pubkey::Pubkey;
use std::ops::DerefMut;
use std::str::FromStr;
use tokio::{fs::File, io::AsyncReadExt};

mod account;
pub use account::*;

mod serve_csv;
pub use serve_csv::{server_latest_delegated_positions_as_csv, server_latest_positions_as_csv};

mod legacy;
pub use legacy::delegated_stakes;

mod stats;
pub use stats::vehnt_positions_stats;

mod timer;
pub use timer::get_positions;

#[derive(Debug)]
pub struct Memory {
    data: HashMap<i64, Arc<positions::AllPositionsData>>,
    pub position: HashMap<Pubkey, positions::Position>,
    pub latest_data: Arc<positions::AllPositionsData>,
    pub positions_by_owner: HashMap<Pubkey, Account>,
}

impl Memory {
    fn latest_delegated_positions_file(&self) -> String {
        format!(
            "./delegated_positions_{}.csv",
            self.latest_data.vehnt.timestamp
        )
    }

    fn latest_positions_file(&self) -> String {
        format!("./positions_{}.csv", self.latest_data.vehnt.timestamp)
    }

    #[allow(unused)]
    pub async fn new(latest_data: positions::AllPositionsData) -> Result<Memory> {
        let mut memory = Self {
            data: HashMap::new(),
            position: HashMap::new(),
            latest_data: Arc::new(positions::AllPositionsData::default()),
            positions_by_owner: HashMap::new(),
        };
        memory.update_data(latest_data).await?;
        Ok(memory)
    }

    async fn remove_csv(&self, path: String) -> Result {
        tokio::fs::remove_file(path).await?;
        Ok(())
    }

    fn write_latest_to_csv(&self) -> Result {
        use csv::Writer;

        #[derive(serde::Serialize)]
        struct Position<'a> {
            pub position_key: &'a str,
            pub owner: &'a str,
            pub hnt_amount: u64,
            pub start_ts: i64,
            pub genesis_end_ts: i64,
            pub end_ts: i64,
            pub duration_s: i64,
            pub vehnt: u128,
            pub lockup_type: &'a positions::LockupType,
            pub delegated_position_key: Option<&'a str>,
            pub delegated_sub_dao: Option<SubDao>,
            pub delagated_last_claimed_epoch: Option<u64>,
            pub delegated_pending_rewards: Option<u64>,
        }

        let mut position_wtr = Writer::from_path(self.latest_positions_file())?;
        let mut delegated_position_wtr = Writer::from_path(self.latest_delegated_positions_file())?;
        for position in self.latest_data.vehnt.positions.iter() {
            if let Some(delegated) = &position.delegated {
                position_wtr.serialize(Position {
                    position_key: &position.position_key,
                    owner: &position.owner,
                    hnt_amount: position.hnt_amount,
                    start_ts: position.start_ts,
                    genesis_end_ts: position.genesis_end_ts,
                    end_ts: position.end_ts,
                    duration_s: position.duration_s,
                    vehnt: position.vehnt,
                    lockup_type: &position.lockup_type,
                    delegated_position_key: Some(&delegated.delegated_position_key),
                    delegated_sub_dao: Some(delegated.sub_dao),
                    delagated_last_claimed_epoch: Some(delegated.last_claimed_epoch),
                    delegated_pending_rewards: Some(delegated.pending_rewards),
                })?;
            } else {
                position_wtr.serialize(Position {
                    position_key: &position.position_key,
                    owner: &position.owner,
                    hnt_amount: position.hnt_amount,
                    start_ts: position.start_ts,
                    genesis_end_ts: position.genesis_end_ts,
                    end_ts: position.end_ts,
                    duration_s: position.duration_s,
                    vehnt: position.vehnt,
                    lockup_type: &position.lockup_type,
                    delegated_position_key: None,
                    delegated_sub_dao: None,
                    delagated_last_claimed_epoch: None,
                    delegated_pending_rewards: None,
                })?;
            }
        }
        for position in self.latest_data.vehnt.delegated_positions.iter() {
            delegated_position_wtr.serialize(position)?;
        }
        Ok(())
    }

    async fn pull_latest_data(
        rpc_client: &Arc<RpcClient>,
        epoch_summaries: Arc<Mutex<epoch_info::Memory>>,
        position_owner_map: &mut positions::PositionOwners,
    ) -> Result<positions::AllPositionsData> {
        let epoch_summaries = {
            let lock = epoch_summaries.lock().await;
            lock.latest_data.clone()
        };
        let mut latest_data =
            positions::get_data(rpc_client, epoch_summaries, position_owner_map).await?;
        latest_data.scale_down();
        Ok(latest_data)
    }

    async fn update_data(&mut self, latest_data: positions::AllPositionsData) -> Result {
        print!("Updating data...");
        use chrono::Utc;
        let previous_file = self.latest_delegated_positions_file();
        let latest_data = Arc::new(latest_data);
        self.latest_data = latest_data.clone();

        // organize into map of positions pubkey to full position data
        self.position = latest_data
            .vehnt
            .positions
            .iter()
            .map(|p| (Pubkey::from_str(&p.position_key).unwrap(), p.clone()))
            .collect();

        // organize into map of owner pubkey to [position pubkey]
        let mut positions_by_owner: HashMap<Pubkey, Account> = HashMap::new();
        for position in latest_data.vehnt.positions.iter() {
            let owner = Pubkey::from_str(&position.owner)?;
            let position = Pubkey::from_str(&position.position_key)?;
            if let Some(entry) = positions_by_owner.get_mut(&owner) {
                entry.push_entry(&self.position, position)?;
            } else {
                positions_by_owner.insert(
                    owner,
                    //TODO: stuck here
                    Account::initialize_with_element(&self.position, position)?,
                );
            }
        }
        self.positions_by_owner = positions_by_owner;

        // start a new Hashmap of all cached positions
        let mut data = HashMap::new();
        data.insert(latest_data.vehnt.timestamp, latest_data.clone());

        // Only keep data that is less than 16 minutes old
        let current_time = Utc::now().timestamp();
        for (key, value) in &self.data {
            if value.vehnt.timestamp > current_time - 60 * 16 {
                data.insert(*key, value.clone());
            }
        }
        println!(" History contains {} entries", data.len());
        self.data = data;
        self.write_latest_to_csv()?;
        if let Err(e) = self.remove_csv(previous_file).await {
            println!(
                "Failed to remove previous csv: {}. This is expected at first boot.",
                e
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct PositionParams {
    timestamp: Option<i64>,
    start: Option<usize>,
    limit: Option<usize>,
}

pub async fn vehnt_positions(
    Extension(memory): Extension<Arc<Mutex<Option<Memory>>>>,
    query: Query<PositionParams>,
) -> HandlerResult {
    const DEFAULT_LIMIT: usize = 500;
    let query = query.0;
    let data = {
        let memory = memory.lock().await;
        if memory.is_none() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                DATA_NOT_INIT_MSG.to_string(),
            ));
        }
        let memory = memory.as_ref().unwrap();
        if let Some(timestamp) = query.timestamp {
            if let Some(data) = memory.data.get(&timestamp) {
                Ok(data.clone())
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    format!("Data not found for timestamp = {timestamp}"),
                ))
            }
        } else {
            Ok(memory.latest_data.clone())
        }
    }?;

    let start = query.start.map_or(0, |start| start);
    if start > data.vehnt.positions.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Start index {start} is greater than the total number of positions {total}",
                total = data.vehnt.positions.len()
            ),
        ));
    }

    let max_data = data.vehnt.positions.len() - start;
    let limit = query.limit.map_or(DEFAULT_LIMIT, |limit| {
        limit.min(DEFAULT_LIMIT).min(max_data)
    });

    let mut positions = Vec::with_capacity(limit);
    positions.resize(limit, positions::Position::default());
    positions.clone_from_slice(&data.vehnt.positions[start..start + limit]);

    #[derive(Default, Debug, serde::Serialize)]
    pub struct DelegatedData {
        pub timestamp: i64,
        pub positions: Vec<positions::Position>,
        pub positions_total_len: usize,
    }

    let data = DelegatedData {
        positions_total_len: data.vehnt.positions_total_len,
        positions,
        timestamp: data.vehnt.timestamp,
    };

    Ok(response::Json(json!(data)))
}

pub async fn vehnt_position(
    Extension(memory): Extension<Arc<Mutex<Option<Memory>>>>,
    Path(position): Path<String>,
) -> HandlerResult {
    if let Ok(pubkey) = Pubkey::from_str(&position) {
        let memory = memory.lock().await;
        if memory.is_none() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                DATA_NOT_INIT_MSG.to_string(),
            ));
        }
        let memory = memory.as_ref().unwrap();
        if let Some(position) = memory.position.get(&pubkey) {
            Ok(response::Json(json!(position)))
        } else {
            Err((
                StatusCode::NOT_FOUND,
                format!("\"{position}\" is not a known position from the voter stake registry"),
            ))
        }
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("\"{position}\" is not a valid base58 encoded Solana pubkey"),
        ))
    }
}
