CREATE TRIGGER {{t}}_no_replace BEFORE INSERT ON {{t}}
  WHEN EXISTS (SELECT 1 FROM {{t}} o WHERE {{cond}})
  BEGIN SELECT RAISE(ABORT, 'doubleentry: REPLACE on {{t}} refused (a row with its key exists)'); END;
