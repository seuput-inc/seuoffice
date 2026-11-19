// Shared by every probe page. Each record goes to the shell's `probe-sink` scheme as a GET
// with the JSON in the query, so it arrives even where a POST body to a custom scheme does not.
// Records carry a sequence number: requests are not delivered in order.
;(function () {
  'use strict'
  var windowsShape = location.protocol === 'http:' || location.protocol === 'https:'
  var base = function (name) {
    return windowsShape ? 'http://' + name + '.localhost/' : name + '://localhost/'
  }
  var t0 = performance.now()
  var seq = 0
  function post(record) {
    var text = JSON.stringify(record)
    if (text.length > 6000) {
      text = JSON.stringify({
        ev: record.ev,
        seq: record.seq,
        truncated: text.length,
        head: text.slice(0, 1500),
      })
    }
    var url = base('probe-sink') + 'log?d=' + encodeURIComponent(text)
    var ignore = function () {}
    try {
      fetch(url, { mode: 'no-cors', cache: 'no-store' }).then(ignore, ignore)
    } catch (error) {
      new Image().src = url
    }
  }
  var P = {
    windowsShape: windowsShape,
    base: base,
    log: function (ev, data) {
      var record = {
        ev: ev,
        seq: seq++,
        pt: Math.round((performance.now() - t0) * 10) / 10,
        href: location.href,
      }
      if (data) for (var key in data) if (key !== 'ev' && key !== 'seq') record[key] = data[key]
      post(record)
    },
    done: function (data) {
      P.log('page.done', data)
    },
    sleep: function (ms) {
      return new Promise(function (resolve) {
        setTimeout(resolve, ms)
      })
    },
    until: async function (test, ms, step) {
      var end = performance.now() + (ms || 5000)
      while (performance.now() < end) {
        var value = await test()
        if (value) return value
        await P.sleep(step || 25)
      }
      return undefined
    },
    loaded: function () {
      return document.readyState === 'complete'
        ? Promise.resolve()
        : new Promise(function (resolve) {
            addEventListener(
              'load',
              function () {
                resolve()
              },
              { once: true },
            )
          })
    },
    navType: function () {
      var entry = performance.getEntriesByType('navigation')[0]
      return entry ? entry.type : null
    },
  }
  addEventListener('error', function (event) {
    P.log('page.error', {
      message: String(event.message),
      source: event.filename,
      line: event.lineno,
    })
  })
  addEventListener('unhandledrejection', function (event) {
    P.log('page.rejection', {
      reason: String(event.reason && event.reason.stack ? event.reason.stack : event.reason),
    })
  })
  window.P = P
})()
