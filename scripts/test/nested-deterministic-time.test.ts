import { test } from "bun:test";
import { runScript } from "./run-script";

test(
  "nested-deterministic-time",
  async () => runScript("nested-deterministic-time"),
  1_800_000,
);
