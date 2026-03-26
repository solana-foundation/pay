import React, { FC, ReactNode, useEffect, useState } from 'react';
import { Theme, ThemeContext } from '../../hooks/useTheme';

export interface ThemeProviderProps {
    children: ReactNode;
}

export const ThemeProvider: FC<ThemeProviderProps> = ({ children }) => {
    const query = '(prefers-color-scheme: light)';
    const [theme, setTheme] = useState<Theme>(() =>
        typeof window !== 'undefined' && window.matchMedia && window.matchMedia(query).matches ? 'light' : 'dark'
    );

    useEffect(() => {
        if (window.matchMedia) {
            const listener = (event: MediaQueryListEventMap['change']) => setTheme(event.matches ? 'light' : 'dark');

            window.matchMedia(query).addEventListener('change', listener);
            return () => window.matchMedia(query).removeEventListener('change', listener);
        }
    }, []);

    useEffect(() => {
        document.documentElement.classList.add(theme);
        document.documentElement.style.visibility = 'visible';
        return () => document.documentElement.classList.remove(theme);
    }, [theme]);

    return <ThemeContext.Provider value={{ theme, setTheme }}>{children}</ThemeContext.Provider>;
};
