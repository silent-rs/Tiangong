import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import { ToastProvider } from './components/Toast'
import { useTheme } from './hooks/useTheme'
import { ConfigApp } from './pages/ConfigApp'

function Root() {
  useTheme();
  return (
    <ToastProvider>
      <ConfigApp />
    </ToastProvider>
  );
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <Root />
  </StrictMode>,
)
