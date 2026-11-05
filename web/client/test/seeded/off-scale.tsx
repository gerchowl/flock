// Seeded violation for tests/design-rules.test.ts: 14 px is not a step of the
// 4 px spacing scale, so `tsc -p test/seeded` must reject this file.
import { Stack } from "../../src/layout/primitives";

export const OffScale = () => <Stack space="14" />;
