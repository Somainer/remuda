/**
 * Evidence harness: render the production WorkflowTimelineCard with the
 * literal observations the Rust producer emitted for a real claude 2.1.221
 * run (dumped via the `workflow_live_dump` example). No data is invented.
 */
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { WorkflowTimelineCard } from "../src/features/session/workflow/WorkflowTimelineCard";
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../src/types/generated";
import data221 from "./real-221.json";
import dataLive from "./real-live.json";

import "./page.css";

type Data = Record<string, unknown>;

function Frame({
  title,
  run,
  phases,
  members,
}: {
  title: string;
  run: WorkflowRunPayload;
  phases: WorkflowPhasePayload[];
  members: WorkflowMemberPayload[];
}) {
  return (
    <section className="frame">
      <h2>{title}</h2>
      <WorkflowTimelineCard
        run={run as unknown as WorkflowRunPayload}
        phases={phases as unknown as WorkflowPhasePayload[]}
        members={members as unknown as WorkflowMemberPayload[]}
      />
    </section>
  );
}

function sections(data: Data): { run: WorkflowRunPayload; phases: WorkflowPhasePayload[]; members: WorkflowMemberPayload[] }[] {
  return [
    {
      run: data.liveRun as WorkflowRunPayload,
      phases: data.livePhases as WorkflowPhasePayload[],
      members: data.liveMembers as WorkflowMemberPayload[],
    },
    {
      run: data.doneRun as WorkflowRunPayload,
      phases: data.donePhases as WorkflowPhasePayload[],
      members: data.doneMembers as WorkflowMemberPayload[],
    },
  ];
}

const sources: { name: string; data: Data }[] = [
  { name: "live capture 2026-09-15 — wfprod-evidence-1 (wf_fcfd9e24-f8f)", data: dataLive as Data },
  { name: "recorded spike — spike-wf-1 / claude 2.1.221 (wf_c3422384-cb1)", data: data221 as Data },
];

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {sources.map((source) => (
      <div key={source.name}>
        <h1 className="source">{source.name}</h1>
        {sections(source.data).map((section, i) => (
          <Frame
            key={i}
            title={i === 0 ? "Running — live line «当前 phase: agent»" : "Completed — auto-collapsed summary"}
            run={section.run}
            phases={section.phases}
            members={section.members}
          />
        ))}
      </div>
    ))}
  </StrictMode>,
);
