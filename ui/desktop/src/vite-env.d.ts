/// <reference types="vite/client" />

declare module '*.json' {
  const value: Record<string, unknown>;
  export default value;
}

declare module '*.png' {
  const value: string;
  export default value;
}

declare module '*.jpg' {
  const value: string;
  export default value;
}

declare module '*.jpeg' {
  const value: string;
  export default value;
}

declare module '*.gif' {
  const value: string;
  export default value;
}

declare module '*.svg' {
  const value: string;
  export default value;
}

declare module '*.mp3' {
  const value: string;
  export default value;
}

declare module '*.mp4' {
  const value: string;
  export default value;
}

declare module '*.md?raw' {
  const value: string;
  export default value;
}

declare global {
  interface Window {
    isCreatingRecipe?: boolean;
  }

  // Build-time flag baked in by vite.renderer.config.mts from
  // WARMACHINE_DESKTOP_ONPREM. True only in the on-prem desktop variant.
  const __WARMACHINE_DESKTOP_ONPREM__: boolean;

  interface WindowEventMap {
    'add-active-session': CustomEvent<{
      sessionId: string;
      initialMessage?: string;
    }>;
    'clear-initial-message': CustomEvent<{
      sessionId: string;
    }>;
    responseStyleChanged: CustomEvent;
    'session-created': CustomEvent<{ session?: import('./types/session').Session }>;
    'session-deleted': CustomEvent<{ sessionId: string }>;
    'session-renamed': CustomEvent<{
      sessionId: string;
      newName: string;
      userInitiated?: boolean;
    }>;
  }
}

export {};
