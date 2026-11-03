import { expect, test } from "bun:test";
import { actionItems, parseMilestones } from "../../src/milestone";

test("presents CLI actions without changing their labels or priority", () => {
  const actions = [
    { id: "confirm", label: "Confirm agent claim", description: "Confirm the claim", primary: true, destructive: false },
    { id: "invalidate", label: "Invalidate claim", description: "Clear the claim", primary: false, destructive: true },
  ];
  const [first, second] = actionItems(actions);
  expect(first.label).toBe("Confirm agent claim");
  expect(first.description).toBe("");
  expect(second.description).toBe("Advanced");
  expect(second.action.destructive).toBe(true);
  expect(parseMilestones(JSON.stringify({ version: 1, milestones: [{ id: "m", title: "Test", state: "reached", counts: false, pending_confirmation: true, evidence_alternatives: false, evidence: [], actions }] }))[0].actions).toEqual(actions);
  expect(() => parseMilestones('{"version":2,"milestones":[]}')).toThrow();
  expect(() => parseMilestones('{"version":1,"milestones":[{"id":"m"}]}')).toThrow();
});
