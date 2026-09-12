fn main() {
    let _ = zeroship_data_orm::exec::run_sql;
    let _ = zeroship_data_orm::transaction::driver::run_operation;
    let _ = zeroship_data_orm::transaction::run_on_tx_conn;
}
