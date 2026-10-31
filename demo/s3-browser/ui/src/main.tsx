import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClientProvider } from '@tanstack/react-query'
import { queryClient } from './queries/client'
import Root from './Root'
import ThemeProvider from './ThemeProvider'
import '@fontsource-variable/manrope'
import '@fontsource-variable/jetbrains-mono'
import './theme.css'
import { installThemeVars } from './themeVars'

installThemeVars()

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <Root />
      </ThemeProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
