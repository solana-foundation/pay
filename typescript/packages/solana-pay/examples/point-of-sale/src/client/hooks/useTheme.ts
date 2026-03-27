import { createContext, useContext } from 'react';

export type Theme = 'light' | 'dark';

export interface ThemeContextState {
    theme: Theme;
    setTheme(theme: Theme): void;
}

export const ThemeContext = createContext<ThemeContextState>({} as ThemeContextState);

export function useTheme(): ThemeContextState {
    return useContext(ThemeContext);
}
