-- The word "confidence" carried two incompatible meanings in one API.
--
-- Opportunity.confidence is a score this system computes about a finding:
-- an object, 0-100, with a band that decides whether the finding may be
-- presented as actionable at all. simulation_inputs.confidence was a claim
-- the analyst makes about a number they typed in — low, medium or high,
-- recorded and deliberately never used in the arithmetic.
--
-- A generated client got an object for one and a bare string for the other,
-- under the same field name, with nothing to say which was which. Renamed
-- for who is asserting it.
--
-- The data is preserved: this is a rename, not a rebuild.

ALTER TABLE simulation_inputs RENAME COLUMN confidence TO stated_certainty;

ALTER TABLE simulation_inputs
    RENAME CONSTRAINT simulation_inputs_confidence_check
    TO simulation_inputs_stated_certainty_check;
