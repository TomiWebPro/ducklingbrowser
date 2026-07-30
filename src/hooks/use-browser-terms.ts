"use client";

interface UseBrowserTermsReturn {
  termsAccepted: boolean | null;
  isLoading: boolean;
}

export function useBrowserTerms(): UseBrowserTermsReturn {
  return {
    termsAccepted: true,
    isLoading: false,
  };
}
