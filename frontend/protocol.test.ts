import { expectTypeOf, it } from "vitest";
import { request } from "./api";
import type { FullOutput } from "./protocol";
import type { RuntimeState } from "./types";

it("ties response types and required arguments to their RPC method", () => {
  expectTypeOf(request<"output">).returns.toEqualTypeOf<Promise<FullOutput>>();
  expectTypeOf(request<"runtime_state">).returns.toEqualTypeOf<
    Promise<RuntimeState | null>
  >();
  if (false) {
    // @ts-expect-error Unknown methods must be added to the protocol first.
    void request("shell");
    // @ts-expect-error Execution requires a captured graph document.
    void request("start_run", { graph: "demo" });
    // @ts-expect-error Review decisions are a closed set.
    void request("review", { run_id: "live", review_id: "r", decision: "yes" });
    // @ts-expect-error Output requests require a node id.
    void request("output", { graph: "demo", run_id: "r" });
    // @ts-expect-error Callers cannot claim that bootstrap returns a string.
    void request<string>("bootstrap");
  }
});
