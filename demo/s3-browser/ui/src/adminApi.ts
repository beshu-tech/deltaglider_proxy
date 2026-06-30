// Admin API client — re-export barrel.
//
// The implementation lives in ./adminApi/<domain>.ts, split by domain for
// navigability. This barrel preserves the historical `from '../adminApi'`
// import surface so consumers don't change. Add a new `export *` line here
// whenever a new domain module is added under ./adminApi/.
export * from './adminApi/core';
export * from './adminApi/users';
export * from './adminApi/cannedPolicies';
export * from './adminApi/groups';
export * from './adminApi/whoami';
export * from './adminApi/scanner';
export * from './adminApi/logs';
export * from './adminApi/backup';
export * from './adminApi/backends';
export * from './adminApi/recovery';
export * from './adminApi/replication';
export * from './adminApi/lifecycle';
export * from './adminApi/jobs';
export * from './adminApi/maintenance';
export * from './adminApi/authProviders';
export * from './adminApi/mappingRules';
export * from './adminApi/externalIdentities';
export * from './adminApi/audit';
export * from './adminApi/eventOutbox';
export * from './adminApi/sessions';
export * from './adminApi/bulkObjects';
export * from './adminApi/deltaEfficiency';
export * from './adminApi/bucketScan';
