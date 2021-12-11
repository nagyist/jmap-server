use store::{
    serialize::serialize_stored_key, AccountId, ArrayPos, CollectionId, DocumentId, FieldId,
    StoreError, StoreGet,
};

use crate::RocksDBStore;

impl StoreGet for RocksDBStore {
    fn get_stored_value(
        &self,
        account: AccountId,
        collection: CollectionId,
        document: DocumentId,
        field: FieldId,
        pos: ArrayPos,
    ) -> crate::Result<Option<Vec<u8>>> {
        self.db
            .get_cf(
                &self.db.cf_handle("values").ok_or_else(|| {
                    StoreError::InternalError("No values column family found.".into())
                })?,
                &serialize_stored_key(account, collection, document, field, pos),
            )
            .map_err(|e| StoreError::InternalError(e.into_string()))
    }
}