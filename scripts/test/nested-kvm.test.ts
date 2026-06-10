import { test } from "bun:test";
import { runScript } from "./run-script";

test("nested-kvm", async () => runScript("nested-kvm"), 1_800_000);
