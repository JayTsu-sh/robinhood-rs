SET @def = '{
  "apiVersion":"hsm.storage.io/v1alpha3","kind":"Policy","metadata":{"name":""},
  "spec":{"target":{"filesystem":"execution-fixture","objectType":"file"},
    "trigger":{"type":"periodic","interval":"1h"},"objective":{"type":"exhaust"},
    "rules":{"entries":[{"match":{"field":"object.size","op":"gte","value":64},"decision":"execute","action":{"type":"lustre.hsm.archive","params":{"archiveId":1}}}]},
    "execution":{"profileRef":"builtin-preview","budget":{"run":{"maxOperations":10,"maxSubmitAttempts":10,"maxObjectLogicalBytes":"1GiB"}}},
    "governance":{"authorizationRef":"builtin-preview","approvalPolicyRef":"builtin-preview"},"enabled":false}
}';
START TRANSACTION;
SET @backlog = JSON_SET(@def,'$.metadata.name','qa-queue-backlog');
INSERT INTO policies (name,definition,revision,enabled) VALUES ('qa-queue-backlog',@backlog,1,FALSE);
SET @policy = LAST_INSERT_ID();
INSERT INTO policy_revisions (policy_id,revision,definition) VALUES (@policy,1,@backlog);
INSERT INTO policy_runs (policy_id,revision,state,updated_at)
SELECT @policy,1,'queued','2020-01-01 00:00:00.000000' FROM seq_1_to_120;
SET @controls = JSON_SET(@def,'$.metadata.name','qa-run-controls','$.spec.enabled',JSON_EXTRACT('true','$'));
INSERT INTO policies (name,definition,revision,enabled) VALUES ('qa-run-controls',@controls,1,TRUE);
SET @policy = LAST_INSERT_ID();
INSERT INTO policy_revisions (policy_id,revision,definition) VALUES (@policy,1,@controls);
INSERT INTO policy_runs (policy_id,revision,state,requested_by) VALUES (@policy,1,'queued','qa-fixture');
INSERT INTO policy_runs (policy_id,revision,state,requested_by,candidate_count,estimated_bytes,started_at) VALUES
(@policy,1,'awaiting_approval','qa-fixture',1,100,UTC_TIMESTAMP(6));
COMMIT;
