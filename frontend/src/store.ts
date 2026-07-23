import { create } from "zustand";
import {
  createBreathSlice,
  createConnectionSlice,
  createDialogSlice,
  createMediaSlice,
  createProjectSlice,
  createRunSlice,
  createSessionSlice,
  createToolSlice,
  createUiSlice,
  type AppState,
} from "./store/slices";

/**
 * Public application store. The flat selector/action surface is intentionally
 * preserved while each concern is implemented by an explicit owned slice.
 */
export const useStore = create<AppState>()((...args) => ({
  ...createConnectionSlice(...args),
  ...createSessionSlice(...args),
  ...createRunSlice(...args),
  ...createToolSlice(...args),
  ...createProjectSlice(...args),
  ...createDialogSlice(...args),
  ...createMediaSlice(...args),
  ...createBreathSlice(...args),
  ...createUiSlice(...args),
}));

export type { AppState } from "./store/slices";
