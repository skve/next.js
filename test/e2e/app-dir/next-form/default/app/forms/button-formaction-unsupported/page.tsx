'use client'
import * as React from 'react'
import Form from 'next/form'

export default function Home({
  searchParams,
}: {
  searchParams: Record<string, string>
}) {
  const attribute = searchParams.attribute as string | undefined
  return (
    <div
      onSubmit={(e) => {
        // should fire if the form let the event bubble up
        if (e.defaultPrevented) {
          console.log('incorrect: default submit behavior was prevented')
        } else {
          console.log('correct: default submit behavior was not prevented')
          e.preventDefault() // this submission will do something stupid, we don't want it to actually go through.
        }
      }}
    >
      <Form action="/search" id="search-form">
        <input name="query" />
        <button
          type="submit"
          formAction="/search"
          formEncType={
            attribute === 'formEncType' ? 'multipart/form-data' : undefined
          }
          formMethod={attribute === 'formMethod' ? 'post' : undefined}
          formTarget={attribute === 'formTarget' ? 'bloop' : undefined}
        >
          Submit
        </button>
      </Form>
    </div>
  )
}