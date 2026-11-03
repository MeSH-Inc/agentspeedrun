/** The CLI owns lifecycle and policy decisions; the host only presents them. */
export type GuidedAction = { id: string; label: string; description: string; primary: boolean; destructive: boolean };
export type GuidedMilestone = { id: string; key: string | null; title: string; state: string; counts: boolean; pending_confirmation: boolean; actions: GuidedAction[]; evidence_alternatives: boolean; evidence: Array<{ flag: string; label: string }> };

export function parseMilestones(output: string): GuidedMilestone[] {
  const data: unknown = JSON.parse(output);
  const record = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
  if (!record(data) || data.version !== 1 || !Array.isArray(data.milestones)) throw new Error("Unsupported milestone actions");
  for (const milestone of data.milestones) {
    if (!record(milestone) || typeof milestone.id !== "string" || typeof milestone.title !== "string" || typeof milestone.state !== "string" || typeof milestone.counts !== "boolean" || typeof milestone.pending_confirmation !== "boolean" || typeof milestone.evidence_alternatives !== "boolean" || !Array.isArray(milestone.actions) || !Array.isArray(milestone.evidence)) throw new Error("Invalid milestone actions");
    for (const action of milestone.actions) {
      if (!record(action) || typeof action.id !== "string" || !["accept", "reject", "split", "confirm", "invalidate"].includes(action.id) || typeof action.label !== "string" || typeof action.description !== "string" || typeof action.primary !== "boolean" || typeof action.destructive !== "boolean") throw new Error("Invalid milestone action");
    }
    for (const evidence of milestone.evidence) {
      if (!record(evidence) || typeof evidence.flag !== "string" || !["--url", "--ci", "--local-test", "--stripe"].includes(evidence.flag) || typeof evidence.label !== "string") throw new Error("Invalid milestone evidence");
    }
  }
  return data.milestones as GuidedMilestone[];
}

export function actionItems(actions: GuidedAction[]) {
  return actions.map((action) => ({ label: action.label, description: action.primary ? "" : "Advanced", detail: action.description, action }));
}
