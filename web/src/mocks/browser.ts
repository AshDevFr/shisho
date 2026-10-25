import { setupWorker } from "msw/browser";
import { demoHandlers } from "./demo";
import { handlers } from "./handlers";

// Used in the browser when running `npm run dev:mock`: the demo data first,
// the shared defaults behind it.
// Requires the service worker file: `npx msw init public/ --save`.
export const worker = setupWorker(...demoHandlers, ...handlers);
