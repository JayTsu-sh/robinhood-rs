SET @name = 'history-files-container-validation';
SET @next_name = CONCAT(@name, '-current');
SET @definition = '{
  "apiVersion":"hsm.storage.io/v1alpha3","kind":"Policy","metadata":{"name":""},
  "spec":{"target":{"filesystem":"execution-fixture","objectType":"file"},
    "trigger":{"type":"periodic","interval":"1h"},"objective":{"type":"exhaust"},
    "rules":{"entries":[{"match":{"field":"object.size","op":"gte","value":64},"decision":"execute","action":{"type":"lustre.hsm.archive","params":{"archiveId":1}}}]},
    "execution":{"profileRef":"builtin-preview","budget":{"run":{"maxOperations":10,"maxSubmitAttempts":10,"maxObjectLogicalBytes":"1GiB"}}},
    "governance":{"authorizationRef":"builtin-preview","approvalPolicyRef":"builtin-preview"},"enabled":false}
}';
SET @original = JSON_SET(@definition,'$.metadata.name',@name);
SET @current = JSON_SET(@definition,'$.metadata.name',@next_name);
INSERT INTO policies (name,definition,revision,enabled) VALUES (@next_name,@current,2,FALSE);
SET @policy = LAST_INSERT_ID();
INSERT INTO policy_revisions (policy_id, revision, definition) VALUES (@policy,1,@original),(@policy,2,@current);
INSERT INTO policy_runs (policy_id,revision,state,requested_by,estimated_bytes) VALUES (@policy,1,'finished','operator',9007199254740993);
SET @run = LAST_INSERT_ID();
SET @plan = '{"sourcePool":"hot","targetPool":"cold","stripeCount":2,"stripeSize":1048576}';
SET @evidence = '{"source":"lustre_layout","pool":"cold","ostIndices":[8,9],"observedAtMicros":1795852800000000}';
INSERT INTO policy_operations (run_id,filesystem,fid,action,state,reason,attempted,size,migration_plan,completion_evidence,finished_at,checked_at)
SELECT @run,@name,UNHEX(CONCAT('0000000200000401',LPAD(HEX(seq),8,'0'),'00000000')),'lustre.pool_migrate',
       IF(seq=102,'failed','completed'),IF(seq=102,'layout_swap_failed',NULL),TRUE,9007199254740993,@plan,
       IF(seq=102,NULL,@evidence),IF(seq>=102,NULL,UTC_TIMESTAMP(6)),IF(seq=103,'2026-11-28 06:15:00.000000',NULL)
FROM seq_1_to_103 ORDER BY seq;
SELECT @run AS run_id, COUNT(*) FROM policy_operations WHERE run_id=@run;
