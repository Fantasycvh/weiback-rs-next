import React from 'react'
import ReactDOM from 'react-dom/client'
import { HashRouter } from 'react-router-dom'
import { SnackbarProvider } from 'notistack'
import CssBaseline from '@mui/material/CssBaseline'
import { AppThemeProvider } from './AppThemeProvider'
import App from './App'
import { SnackbarAction } from './components/SnackbarAction'

import '@fontsource/roboto/300.css'
import '@fontsource/roboto/400.css'
import '@fontsource/roboto/500.css'
import '@fontsource/roboto/700.css'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <AppThemeProvider>
      <CssBaseline />
      <SnackbarProvider
        maxSnack={3}
        anchorOrigin={{ vertical: 'top', horizontal: 'right' }}
        action={snackbarId => <SnackbarAction id={snackbarId} />}
      >
        <HashRouter>
          <App />
        </HashRouter>
      </SnackbarProvider>
    </AppThemeProvider>
  </React.StrictMode>
)
