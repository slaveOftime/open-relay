import { Component, type ReactNode } from 'react'
import { Button } from '@/components/ui/button'

type Props = {
  children: ReactNode
}

type State = {
  hasError: boolean
  message: string
}

export default class ErrorBoundary extends Component<Props, State> {
  state: State = {
    hasError: false,
    message: '',
  }

  static getDerivedStateFromError(error: Error): State {
    return {
      hasError: true,
      message: error.message || 'Unexpected error',
    }
  }

  componentDidCatch() {}

  private reset = () => {
    this.setState({ hasError: false, message: '' })
  }

  render() {
    if (!this.state.hasError) return this.props.children

    return (
      <div className="min-h-screen w-full flex items-center justify-center bg-[hsl(var(--background))] px-4">
        <div className="w-full max-w-md rounded-xl border border-[hsl(var(--border))] bg-[hsl(var(--card))] p-5 text-[hsl(var(--foreground))] shadow-sm">
          <h1 className="text-base font-semibold">Something went wrong</h1>
          <p className="mt-2 text-sm text-[hsl(var(--muted-foreground))]">
            The web client hit an unexpected error.
          </p>
          {this.state.message && (
            <p className="mt-2 text-xs text-[hsl(var(--muted-foreground))] break-all">
              {this.state.message}
            </p>
          )}
          <div className="mt-4 flex items-center justify-end gap-2">
            <Button variant="ghost" size="sm" onClick={this.reset}>
              Try Again
            </Button>
            <Button size="sm" onClick={() => window.location.reload()}>
              Reload
            </Button>
          </div>
        </div>
      </div>
    )
  }
}
